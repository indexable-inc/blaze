---
status: record
summary: Flat syntax trees for files nobody edits (#42): the design and its memory and time, measured.
date: 2026-10-22
---
# Flat syntax trees for files nobody edits

2026-10-22, for [#42](https://github.com/indexable-inc/blaze/issues/42) (Glancer item 4,
[glancer.md](glancer.md#item-4-a-flat-syntax-tree-for-files-nobody-edits)). Measured on an
ephemeral AWS host (64 vCPU, Linux, release builds with mimalloc; `heap-profile` builds with
jemalloc for the heap profiles), blaze at `c43c6443bd` (main) and the `flat-syntax` branch,
and for steps (b) and (c) main at `74fb27b7ed` (step (a) merged) and the `flat-cursor` branch on it,
Cranelift, the `bench.py` projects ripgrep, nushell and helix, each a cold start then the
first request. Each claim is **read** (measured, or read in a source), or **unverified**
(our inference).

## Short version

- On main, syntax holds 12.7% of nushell's live heap after the first request (1,164 MB),
  22% of ripgrep's (315 MB) and 8% of helix's (852 MB). Most of it is the green trees of
  whole files (**read**).
- They outlived the parse query's lru of 128 because each body's `body_syntax` held its
  anchor's green node, and that is a piece of its file's tree. Step (a) keeps the anchor's
  text and range and parses it again on demand: the green trees of files fall from 544 MB to
  87 MB on nushell and from 449 MB to 41 MB on helix, and syntax as a whole to about half
  (nushell 676 MB, helix 436 MB) (**read**).
- What that does to the daemon: resident memory right after the first request is 2.1 GB less
  on helix and 1.1 GB less on nushell, and 0.1 to 0.3 GB less once it settles; the time to
  ready and to the first answer does not move; parsing anchors again costs 1 to 2% more CPU
  (**read**).
- The time syntax costs is CPU, not memory: building green trees and walking red ones are
  16% of ripgrep's cold start to the first request and 7% of nushell's on main, 19% and 9%
  with (a), which parses anchors again (**read**).
- Steps (b) and (c), merged: a fork of syntax-tree whose cursor has a second, flat kind of tree
  (arrays in preorder), so `syntax::ast` and all lowering are unchanged and the compiler path
  builds no green tree. Building trees falls from 12.1% of ripgrep's cold-start CPU to 1.9%,
  from 5.4% to 1.1% on nushell and from 2.2% to 0.4% on helix; walking them takes more, and
  together they are a quarter to a third less CPU (19.4% to 14.7% on ripgrep) (**read**).
- What that does to the daemon: the time to ready, the first answer and resident memory do
  not move beyond the spread of two runs (ripgrep keeps 0.1 to 0.3 GB more that mimalloc has
  not yet returned, and answers 0.2 s sooner) (**read**).
- Walking flat trees took a larger share than walking green ones only because building
  trees went: it cost about the same samples. Reading the arrays directly in the compiler
  path's hot walks (ast id maps, `body_syntax`, `syntax::ast`'s accessors) halves it, from
  10.0% to 5.0% of ripgrep's cold-start CPU, 4.9% to 2.05% of nushell's and 1.85% to 0.89%
  of helix's; ready time, the first answer and memory do not move (**read**,
  [below](#walking-the-arrays-without-the-cursor)).

## What syntax holds (step 1, memory)

`harness/heap.py --syntax` gives each live allocation of a jemalloc heap profile to green
nodes, red nodes, span maps, ast id maps, token trees or the lexer by the frames on its
stack, and to the salsa ingredient that made it. After the first request (`request-1`):

| project | live heap | syntax | trees, files | trees, expansions | span maps | ast id maps | token trees | lexer |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep, main | 1,407 MB | 315 MB (22%) | 201 | 37 | 29 | 12 | 16 | 17 |
| ripgrep, (a) | 1,276 MB | 210 MB (16%) | 92 | 42 | 16 | 18 | 20 | 21 |
| nushell, main | 9,177 MB | 1,164 MB (13%) | 544 | 204 | 151 | 117 | 89 | 56 |
| nushell, (a) | 8,519 MB | 676 MB (8%) | 87 | 219 | 157 | 89 | 78 | 42 |
| helix, main | 10,820 MB | 852 MB (8%) | 449 | 159 | 91 | 32 | 57 | 63 |
| helix, (a) | 10,139 MB | 436 MB (4%) | 41 | 158 | 101 | 31 | 57 | 45 |
| ripgrep, (c) | 1,229 MB | 136 MB (11%) | 27 | 37 | 19 | 13 | 16 | 25 |
| nushell, (c) | 8,324 MB | 576 MB (7%) | 16 | 186 | 135 | 93 | 87 | 55 |
| helix, (c) | 10,181 MB | 413 MB (4%) | 27 | 120 | 98 | 45 | 70 | 51 |

All **read**; MB of sampled live bytes, scaled as jeprof scales them. The ripgrep (a) row
is an earlier build of the branch, which differs only in hashing each anchor more than once.
How much of the expansions' trees is live at the report moves by tens of MB between runs:
it is what their lru of 512 holds when the report is taken (an earlier run of (c) on nushell
held 3 MB of them, this one 186 MB). The (c) rows are flat trees (steps (b) and (c) below);
the few MB of green nodes in them are parts of expansions that something asked a green tree
of (**unverified**: the syntax editor builtin derives use). Red nodes are
under 4 MB everywhere: syntax-tree's cursors are transient. After warm-up, before the first request, nushell's syntax is 689 MB on main and
419 MB with (a), helix's 434 and 176 MB. On helix 5.3 GB of the 10.8 GB is `described`
([#55](https://github.com/indexable-inc/blaze/issues/55)), not syntax.

What is left after (a) is mostly macro expansions (`parse_macro_expansion`, lru 512, with
their span maps) and ast id maps that `body_syntax` makes for an anchor parsed alone. With (c)
the files' trees hold 16 to 27 MB against 41 to 92 MB with (a), but the benchmark under steps
(b) and (c) finds a flat tree about as large as a green one, so that is **unverified** as a
property of flat trees: which trees the lrus hold at the report differs between runs. What the
expansions' trees hold is within that spread, and the rest of syntax is not trees.

## What syntax costs (step 1, time)

`harness/cpufold.py --dso <blaze>` folds `perf record -g` of a cold start through the first
request (blaze's own samples only) by what the innermost frame of a kind does:

| work | ripgrep (8,516 samples) | nushell (56,607) |
|---|---:|---:|
| walking red nodes (syntax-tree's cursor) | 8.5% | 3.7% |
| building green trees (builder and node cache) | 7.8% | 3.6% |
| lexing and parsing to events | 6.7% | 3.1% |
| token trees (syntax to and from) | 6.5% | 3.9% |
| ast id maps | 1.3% | 0.5% |
| span maps | 0.8% | 0.4% |
| code generation | 16.8% | 27.6% |
| salsa | 16.0% | 14.1% |
| inference and traits | 13.4% | 26.4% |

All **read**. A flat tree replaces the first two rows; the parser's events stay.

## Where trees are made and kept

| query | what it keeps | lru |
|---|---|---:|
| `EditionedFileId::parse` | the file's green tree | 128 |
| `parse_macro_expansion` | the expansion's green tree and its span map | 512 |
| `ast_id_map` | pointers (kind and range) to items, no tree | 1,024 |
| `real_span_map` | ranges of items, no tree | none |
| `file_item_tree_query` | the item tree, no tree | none |
| `macro_arg` | the token tree of a macro call, no tree | none |
| `body_syntax` (blaze) | on main the anchor's green node; with (a) its text range and a hash | none |
| `Body::with_source_map` | the body and its source map (pointers) | 512 |
| `Body::relative` | the body lowered relative to its anchor | 4,096 |

All **read** in the subtree. Nothing but `parse`, `parse_macro_expansion` and, on main,
`body_syntax` holds a tree; everything else holds pointers (a kind and a range) that are
resolved against a parse when asked.

## What needs a tree after lowering

- **Diagnostics with spans** (type errors, borrowck, wfcheck): a source map gives an
  `AstPtr`, and `to_node` resolves it against the file's parse, made again when evicted.
  blaze's diagnostics go through `with_source_map`, which with (a) finds the anchor by its
  start and hash in the file's parse.
- **Places** (`hir-ty-codegen/places.rs`, panic locations and debug lines): the anchor's
  text, for its row starts. With (a) that is a slice of the file's text, no tree.
- **Relative bodies** (`Body::relative`, the body as if its anchor were a file of its own,
  so an edit above it does not change it): the anchor's own tree, parsed again from its text.
- **Macro inputs**: `macro_arg` lowers the call's syntax to a token tree once and keeps that;
  an attribute macro needs its item's syntax at that moment only.
- **Edit invalidation**: a changed file text reparses the file; queries that hold pointers
  compare equal when the items did not move, and `body_syntax` compares the anchor's hash,
  so an unchanged body backdates.

## Step (a): no tree outlives its query

**What changed.** `BodySyntax` keeps the file's text (shared with `file_text`), the
anchor's range, kind, edition and a 128-bit hash of its green tree. `root()` takes the tree
from a per-thread cache of the last 32, parses the anchor's text alone, or, when that parses
differently (a `macro_rules!` body that means something else at top level), finds it in the
file's parse by range and hash. `RA_CHECK_RELATIVE_BODIES=1` skips the cache and prints each
anchor that falls back. And the daemon now asks salsa to evict lru values once no build and
no tier compile holds a clone of the database (`let_go`): an lru evicts only when a revision
begins, and a daemon nobody edits begins none.

**Measured.** The heap rows marked (a) above. Resident memory after the first request and
time to ready and to the first request, two quiet rounds each, are in the table below.

| project | ready | first request answered | largest process at its peak | worker after the first request | a few seconds later |
|---|---:|---:|---:|---:|---:|
| ripgrep, main | 6.41 / 6.10 s | 9.22 / 8.88 s | 2,476 / 2,466 MB | 2,472 / 2,313 MB | 2,238 / 2,321 MB |
| ripgrep, (a) | 6.25 / 6.07 s | 9.18 / 9.00 s | 2,366 / 2,341 MB | 2,120 / 2,332 MB | 2,044 / 2,273 MB |
| nushell, main | 39.7 / 37.4 s | 66.2 / 54.5 s | 14,158 / 15,118 MB | 11,864 / 12,742 MB | 10,990 / 11,686 MB |
| nushell, (a) | 40.3 / 37.2 s | 58.2 / 54.9 s | 15,256 / 15,291 MB | 11,231 / 11,232 MB | 11,169 / 11,290 MB |
| helix, main | 37.1 / 36.4 s | 128.5 / 128.6 s | 17,991 / 18,047 MB | 14,329 / 14,300 MB | 12,551 / 12,560 MB |
| helix, (a) | 37.9 / 35.9 s | 137.5 / 126.7 s | 17,587 / 17,554 MB | 12,223 / 12,239 MB | 12,273 / 12,331 MB |

Two rounds each (`harness/coldprof.py`, release builds, nothing else running; seconds from
the start of the cold start). All **read**. The time to ready and to the first answer does not
change beyond the spread of two runs. Resident memory right after the first request falls by
2.1 GB on helix and 1.1 GB on nushell; a few seconds later main's has fallen too (the
allocator returns what the build let go of), and what stays is 0.1 to 0.3 GB less. The peak
is the build's, which (a) does not touch: trees are made and let go of during it as before.

**Cost.** Parsing an anchor again costs CPU: on nushell's cold start to the first request,
building green trees goes from 3.6% to 5.4% of blaze's samples, lexing and parsing from 3.1%
to 3.7% (helix 1.2% to 2.2%) (**read**, perf runs beside other work). That is the cost the
flat tree removes.

**Gate.** Unit tests (blaze 138, hir-ty-codegen 13), hir-ide (its 18 known failures),
corpus `--mixed 8` under Cranelift and LLVM (MATCH, 0 of 902 layouts differ), the corpus
again with `RA_CHECK_RELATIVE_BODIES=1 BLAZE_CHECK_PLACES=1`, mixfutures `--mixed 16
--layouts` under both, growth, movedlines (6 of 6), borrowck_ui (as main), the pinned ui
tests (15 of 15) and the ui run-pass claims against `ui-known.txt`: the differences are the
host's (main has the same five).

## Steps (b) and (c): a flat tree behind syntax-tree's cursor

**Representation.** Every node and token in preorder, as arrays: a kind (`u16`), a start
(`u32`), the number after the subtree (`u32`) and the parent (`u32`, its top bit for a
token), 14 bytes an element. The end of an element is the start of the one after its subtree; a token's text is a
slice of the tree's text, kept once. This is the layout of Carbon's parse tree and Zig's AST,
and what Glancer keeps.

**How the parser emits it.** The same `LexedStr::intersperse_trivia` steps syntax-tree's builder
takes: an enter pushes a node and remembers it, an exit writes where its subtree ends, a
token pushes a token. The parser does not change.

**How lowering consumes it.** Unchanged. rowan 0.17's cursor is about 35 functions over
`NodeData` (a green pointer, a parent, an offset). A fork in `crates/syntax-tree`
(we pin every layer; inside the subtree so that the fork builds alone) gives the cursor a second kind of tree: a `SyntaxNode` that is a flat tree's root and an
element's number, with nothing allocated per node, and moves that are arithmetic on the
arrays. `syntax::ast`, `AstPtr`, ast id maps, item trees and body lowering compile and run as
they are. A green tree of a part of a flat one is made only when something asks for it (the
syntax editor that builtin derives use, IDE assists): `green()` builds that part's and keeps
it with the root. `Parse` holds a green tree or a flat one; parses make flat ones, and
`BLAZE_GREEN_TREES=1` makes green ones, to compare. Steps (b) and (c) are the one change:
with the cursor serving flat trees nothing in the compiler path makes a green tree.

**What changed.** rowan 0.17.0 is vendored as published, then given `flat.rs` (the tree
and its builder) and a cursor whose `Ref` is a green node's data or a flat root and a number.
A flat node's first child and next sibling skip whole subtrees by their end; the child at a
range is a binary search of the starts and a climb; `SyntaxNodePtr::to_node` (every
diagnostic span and every lookup of an item by its ast id) is one binary search and one climb
instead of a search at each level; the arrays are boxed at their size, since mimalloc keeps
a block shrunk to more than half of it whole (`mi_realloc` in `libmimalloc-sys` 0.1.49, **read**)
and a doubling vector boxed held up to twice its elements. `syntax`'s `Parse` holds a green
tree or a flat one and its builder makes either; `body_syntax` hashes a flat tree's arrays and
keeps an anchor's subtree as a tree of its own (`clone_subtree`). One test builds 2,000 random
trees both ways and compares every function of the cursor on every element, every range for
the child at a range and the covering element, and every pointer lookup.

**Measured, the trees alone** (`examples/trees.rs` in `syntax`, one thread, nushell's sources
and the 1,384 crates in the host's cargo registry: 43,196 files, 962 MB, 465 million nodes and
tokens, through the cursor as lowering uses it):

| | green | flat |
|---|---:|---:|
| lex, parse and build | 29.7 s | 24.1 s |
| hold all the trees | 6,518 MB | 7,169 MB |
| walk every node and token (kind and range) | 18.7 s | 13.1 s |
| every node alone, as an ast id map does | 8.3 s | 5.2 s |
| one node in 16 found again from its pointer | 6.8 s | 1.9 s |

**Read.** A parse with its tree takes 19% less time (most of it is lexing and parsing to
events, which both share); walking through the cursor is 1.4 to 1.6 times faster and finding
a node from its pointer 3.5 times. A prototype that walked the arrays without the cursor was
10 times faster: the rest is the cursor's values and reference counts. With a parent for
every element, which the cursor needs, a flat tree is 14 bytes an element and 10% larger here
than syntax-tree's, whose node cache shares equal tokens and small nodes.

**Measured, the daemon.** On a cold start to the first request, by what the innermost frame
of a kind does (`harness/cpufold.py`, perf runs of `heap-profile` builds):

| work | ripgrep (a) | ripgrep (c) | nushell (a) | nushell (c) | helix (a) | helix (c) |
|---|---:|---:|---:|---:|---:|---:|
| samples | 8,482 | 8,060 | 64,592 | 52,422 | 106,101 | 99,935 |
| building trees (green, flat) | 12.1% | 1.9% | 5.4% | 1.1% | 2.2% | 0.4% |
| walking them (syntax-tree's cursor) | 7.3% | 12.7% | 3.8% | 5.2% | 1.4% | 2.1% |
| lexing and parsing to events | 6.5% | 7.4% | 3.7% | 3.6% | 1.3% | 1.4% |

All **read**; the nushell (a) run had other work beside it, so its sample count is not
comparable. Building trees all but goes; walking takes a larger share than syntax-tree's cursor
did, which the benchmark above does not show (why: [below](#walking-the-arrays-without-the-cursor);
the two columns are builds at two commits, and building had been most of the rest. Elements as one array of
16-byte structs, tried, were no faster on one thread and 12% larger, so they are not used). Together they fall from 19.4% to 14.7% of ripgrep's samples, from 9.2% to 6.3% of
nushell's and from 3.6% to 2.5% of helix's.

Ready time, the first answer and resident memory, main (with (a)) against (c), two quiet
rounds each (`harness/coldprof.py`, release builds, seconds from the start of the cold start):

| project | ready | first request answered | largest process at its peak | worker after the first request | a few seconds later |
|---|---:|---:|---:|---:|---:|
| ripgrep, main | 6.36 / 6.51 s | 9.26 / 9.37 s | 2,343 / 2,357 MB | 2,228 / 2,112 MB | 2,168 / 2,123 MB |
| ripgrep, (c) | 6.29 / 6.32 s | 9.11 / 9.10 s | 2,441 / 2,407 MB | 2,318 / 2,386 MB | 2,315 / 2,393 MB |
| nushell, main | 38.6 / 37.3 s | 56.3 / 55.3 s | 15,211 / 15,132 MB | 11,216 / 11,205 MB | 10,928 / 11,088 MB |
| nushell, (c) | 37.3 / 37.8 s | 54.7 / 54.9 s | 14,928 / 14,870 MB | 11,149 / 11,194 MB | 11,243 / 11,258 MB |
| helix, main | 38.2 / 36.9 s | 115.4 / 113.1 s | 17,606 / 17,592 MB | 12,195 / 12,183 MB | 12,285 / 12,266 MB |
| helix, (c) | 36.3 / 38.6 s | 110.5 / 119.2 s | 17,794 / 17,573 MB | 12,208 / 12,207 MB | 12,281 / 12,291 MB |

All **read**. Neither the time to ready nor the first answer moves beyond the spread of two
runs; ripgrep's first answer came 0.15 to 0.3 s sooner in all four pairs of this round and an
earlier one. Resident memory is the same on nushell and helix. On ripgrep it is 0.1 to
0.3 GB more although the live heap is less: that is memory mimalloc has been given back and
not yet returned to the system, because with `MIMALLOC_PURGE_DELAY=0` ripgrep settles at
1,915 / 1,901 MB on main and 1,925 / 1,906 MB with (c) (**read**). So (c) saves CPU, not
seconds (**unverified** why: the CPU it saves is spread over 64 threads, and what decides
when the daemon is ready is elsewhere).

**Gate.** Unit tests (blaze 139, hir-ty-codegen 13), hir-ide (its 18 known failures),
corpus `--mixed 8` under Cranelift and LLVM (MATCH, 0 of 908 layouts differ), the corpus with
`RA_CHECK_RELATIVE_BODIES=1 BLAZE_CHECK_PLACES=1`, mixfutures `--mixed 16 --layouts` under
both, growth, movedlines (6 of 6), borrowck_ui (Polonius 1,363 and NLL 1,358, as main), the
pinned ui tests (15 of 15) and the ui run-pass tests against `ui-known.txt` (the host's five
differences, as with (a)); and rust-analyzer's own tests of `parser` (333), `syntax` (56), `mbe` (7),
`syntax-bridge` (9), `hir-expand` (33) and `hir-def` (518), which pass on main and with (c)
alike (`hir-ty`'s tests are in hir-ide in this tree).

**Risks.** Equality of parses (salsa backdates a reparse whose tree is equal) is by arrays and
text instead of green pointers; the gate's edit harnesses (growth, movedlines) cover it. What
asks a flat tree for a green one (`green()`, `replace_with`, the syntax editor) gets a green
tree of that part, made then and kept with the root, so editing behaves as on a green tree
(the test compares `replace_with` and `clone_subtree` on every node).

## Walking the arrays without the cursor

2026-10-22, main at `a7622669fb` and the `flat-walk` branch on it, the same host, builds and
runs as above (perf of `heap-profile` builds, then quiet rounds of release and `dist` builds).

**Why walking took a larger share.** The same binary (main), with flat trees and with
`BLAZE_GREEN_TREES=1`, samples whose innermost frame of a kind walks a tree:

| | ripgrep | nushell | helix |
|---|---:|---:|---:|
| walking green trees | 574 | 2,511 | 1,767 |
| walking flat trees | 638 | 2,469 | 1,777 |
| building green trees, of all samples | 9.5% | 4.5% | 2.0% |
| building flat trees, of all samples | 1.8% | 0.9% | 0.35% |

**Read.** Walking costs flat trees about what it cost green ones (+11%, −2%, +1%): the share
rose because building trees, a large part of the rest, all but went, and the (a) and (c)
columns above are two builds at two commits, so they overstate the difference. What flat
trees did add is 13 to 17% of their walking, in two places. `body_syntax` hashed each body's
whole anchor again: the kept trees were found by their root, and every body made a new copy
of its anchor, so none was ever found. And finding an item from its pointer
(`HasSource::source`, through `node_at_range`) cost 3 to 6 times what it did on green trees.
Walking through the cursor was otherwise cheaper on flat trees (ast id maps 15 to 20% fewer
samples).

**What changed.** The compiler path's hot walks read the arrays and make a cursor only of
what they keep:

- syntax-tree: a flat tree's elements by number (kind, parent, end, range, a token's text,
  children, descendants); `hash_subtree`, which hashes a subtree in place as its copy would be
  hashed; on the cursor `flat()` (the tree and the number), `flat_element`, `to_flat()` (a
  green tree's twin), and `child_where`, `child_token`, `children_where` and `ancestor_where`,
  which scan the arrays of a flat node; `SyntaxNodePtr::from_parts`. `node_at_range` stops
  at the first ancestor larger than the range.
- `AstIdMap::from_source` and `relative_to` walk the elements by number, a block left once
  the walk is past its end, and make cursors only of the items they name.
- `body_syntax` hashes an anchor in place, and a kept tree is also found by the anchor it was
  made of (the file's tree, held weakly, and the anchor's number), so the bodies of an anchor
  hash and copy it once on each thread; `with_source_map` compares kind and length before a
  hash.
- `syntax::ast`'s child, children and token accessors (attributes, items, every typed child)
  scan the arrays; so do `lower_path`'s climb to an enclosing use-tree list and
  `loaded_crates`' identifiers of a file (as text, no longer interned).

One path: these callers have no cursor fallback. A green tree reaching them (what the syntax
editor makes, or every tree under `BLAZE_GREEN_TREES=1`) is made flat first, except in the
accessors, whose green arm is the cursor, for trees the syntax editor makes.

**Measured, the daemon** (`harness/cpufold.py`, as above):

| | ripgrep, main | ripgrep, branch | nushell, main | nushell, branch | helix, main | helix, branch |
|---|---:|---:|---:|---:|---:|---:|
| samples | 6,358 | 6,183 | 50,590 | 51,587 | 96,058 | 96,914 |
| walking trees | 10.0% | 5.0% | 4.9% | 2.05% | 1.85% | 0.89% |
| ast id maps, on the stack | 1.54% | 0.57% | 0.99% | 0.39% | 0.34% | 0.13% |
| body syntax, on the stack | 4.26% | 2.88% | 2.09% | 1.04% | 0.73% | 0.48% |

All **read**. Walking now takes about half what it took green trees (309, 1,058 and 863
samples against 574, 2,511 and 1,767). By caller, on nushell (samples with the function on
the stack, main against the branch): `AstIdMap::from_source` 503 to 203, `relative_to` 431
to 50, `body_syntax` 1,094 to 588, the item tree's lowering 212 to 139, its attributes 152 to
90, `loaded_crates` 108 to 21, `lower_path` 447 to 331. What is left of walking is spread
thin: hashing an anchor the first time a thread meets it (0.16%), pointer lookups (0.15%),
the token converter of macro inputs (`syntax_bridge`, 0.2%, which hands its preorder to
callbacks) and freeing the arrays (0.06%).

Ready time, the first answer and resident memory (`harness/coldprof.py`, quiet rounds, main
and the branch alternating which goes first), release builds:

| project | ready | first request answered | largest process at its peak | worker after the first request | a few seconds later |
|---|---:|---:|---:|---:|---:|
| ripgrep, main | 6.11 / 6.26 s | 8.82 / 8.96 s | 2,441 / 2,404 MB | 2,197 / 2,151 MB | 2,208 / 2,155 MB |
| ripgrep, branch | 6.09 / 6.25 s | 8.80 / 9.06 s | 2,490 / 2,470 MB | 2,213 / 2,183 MB | 2,226 / 2,192 MB |
| nushell, main | 36.1 / 36.9 / 36.3 / 36.7 s | 52.5 / 53.7 / 53.2 / 53.6 s | 14,857 / 14,839 / 14,990 / 15,006 MB | 11,195 / 11,207 / 11,222 / 11,295 MB | 10,820 / 10,858 / 11,256 / 11,344 MB |
| nushell, branch | 35.9 / 37.1 / 36.8 / 37.6 s | 52.9 / 54.3 / 53.9 / 54.8 s | 14,871 / 15,283 / 15,171 / 15,245 MB | 11,246 / 11,224 / 11,156 / 11,255 MB | 11,330 / 10,934 / 10,821 / 10,912 MB |
| helix, main | 34.7 / 35.7 / 36.0 / 35.8 s | 108.9 / 111.0 / 109.9 / 109.4 s | 17,594 / 17,701 / 17,587 / 17,622 MB | 12,210 / 12,155 / 12,188 / 12,252 MB | 12,271 / 12,245 / 12,282 / 12,339 MB |
| helix, branch | 35.5 / 36.1 / 35.4 / 36.4 s | 120.2 / 113.2 / 113.3 / 114.7 s | 17,520 / 17,662 / 17,816 / 17,523 MB | 12,177 / 12,210 / 12,268 / 12,202 MB | 12,261 / 12,279 / 12,338 / 12,254 MB |

`dist` builds (the release profile with thin LTO, as shipped but without PGO):

| project | ready | first request answered | largest process at its peak | worker after the first request | a few seconds later |
|---|---:|---:|---:|---:|---:|
| nushell, main | 37.4 / 36.3 s | 54.0 / 53.1 s | 15,052 / 15,140 MB | 11,267 / 11,139 MB | 10,986 / 11,166 MB |
| nushell, branch | 36.8 / 36.7 s | 53.6 / 53.4 s | 14,887 / 15,185 MB | 11,310 / 11,115 MB | 10,923 / 10,751 MB |
| helix, main | 36.2 / 36.7 s | 106.5 / 108.2 s | 17,697 / 17,664 MB | 12,224 / 12,188 MB | 12,313 / 12,269 MB |
| helix, branch | 36.0 / 36.1 s | 106.3 / 107.5 s | 17,842 / 17,896 MB | 12,142 / 11,994 MB | 12,220 / 12,103 MB |

All **read**. Ready time and memory do not move. With release builds the branch answered
later in every pair: 0.7 s on nushell (1.3%) and 5.6 s on helix (5%), almost all of it in the
code generator's whole-program rounds (helix's round that is started over took 29.2 to
30.1 s on main and 30.9 to 38.5 s on the branch), which walk no syntax. Its perf profiles
differ there only in what was inlined where: functions present in one build and not the
other add up to thousands of samples each way in `hir-ty` and `hir-ty-codegen`, which the
branch hardly touches, while the work under them (the closure lookups that are a quarter of
helix's samples) is the same. With thin LTO, which redoes the inlining across crates, the
difference is gone (helix 106.9 s against 107.4 s on average, nushell 53.5 s against
53.6 s, the started-over round 27.9 s in both). So it was the release build's layout, not the
change (**read**, the cause by elimination). The CPU saved is a few percent of the cold
start spread over 64 threads, and as with (c) it does not shorten the path to the first
answer.

**Gate.** Unit tests, rust-analyzer's tests of `parser`, `syntax`, `mbe`, `syntax-bridge`,
`hir-expand`, `hir-def`, `span` and `syntax-tree` (the new test compares the array walks with the
cursor's on green and flat trees, and `hash_subtree` in place with the copy's hash), hir-ide
(its 18 known failures), the corpus under Cranelift and LLVM (MATCH, 0 of 910 layouts
differ), the corpus with `RA_CHECK_RELATIVE_BODIES=1 BLAZE_CHECK_PLACES=1`, mixfutures under
both, growth, movedlines (6 of 6), norustc (no count above its baseline, and none above
main's), borrowck_ui (Polonius 1,405 and NLL 1,401, as main), the pinned ui tests (15 of 15)
and the ui run-pass tests, whose differences from `ui-known.txt` are the same on main on this
host (**read**).

## Not checked

- bun: not run. Its workspace builds only after bun's own build has made its C and C++
  (`harness/bunprep.py` under `nix develop .#bench-bun`), which the host did not have.
- Files being edited keep a flat tree too: the daemon has no editor's buffers, so nothing
  in blaze tells an edited file from another. `BLAZE_GREEN_TREES=1` makes every tree green.
