---
status: plan
summary: mem-db-def-expand's plan to move hir-expand's macro-expansion queries onto blaze-mem (one frame per query run under Tag::Expand, expansion and definition trees as frame values, memos owned at exact length, worker scratch on span-mapping hit paths), what it asks of lane format's tt, span, syntax-bridge and mbe, and the speed-of-light bound for its temporaries.
date: 2026-10-29
---
# mem-db-def-expand: hir-expand onto blaze-mem

Part of mem-db-def's plan (`docs/v2/plan/lanes/mem/db-def.md`), against mem-db's contract
(`docs/v2/plan/lanes/mem/db.md`, "The contract"). hir-expand belongs to `db-expand-macros`, so
every change is a request: `v2/requests/mem-db-def-expand.md` (E1 to E14 in hir-expand, M the
memo table, T1 and T2 the budget tests, F1 to F9 of lane format's crates, X1 and X2 outside).

## Where each allocation goes

| what | lifetime | allocator | request |
|---|---|---|---|
| an expansion's tree (`macro_expand`'s result, the `;` split) | one `parse_macro_expansion` run | the run's frame (`tt::TopSubtree<&Frame>`) | E2, F1 |
| a `macro_rules!`/`macro` definition's trees | one `decl_macro_expander` run | the run's frame | E5, F1 |
| mbe's matcher and transcriber state | one expansion | the frame (`mem-format-tokens`), output in the caller's frame | E5, F7 |
| the attribute input's cfg walk: attribute list, inserted leaves, fixup maps | one `macro_arg` run | frame vectors and `hashbrown` maps, leaves inline (`SmallVec<[Leaf; 3]>`) | E3, F5 |
| a builtin derive's shape (fields, variants, generics, where clauses) and its reparse span map | one expansion | the frame | E11 |
| fn-like builtins' text (`concat!`, `assert!`, `format_args_nl!`, byte escapes) | one call | the worker's string or bytes (`scratch::with_string`, `with_bytes`) | E12 |
| `real_span_map`'s anchors | one run | the frame; the radix sort's scratch the worker's | E8 |
| `syntax_error_ranges`' sort scratch | one run | the worker's `Scratch<TextRange>` (applied) | E7 |
| an eager call's argument span map | one eager call, nested | a free list of maps per worker (`ArgMap`), cleared and reused | E13, F3 |
| `expand_speculative`, `find_derive_range` (IDE, outside any run) | one call | a `frame::task(Tag::Expand, ..)` of their own | E10, E14 |
| span upmapping, `ExpansionInfo`, `InFile` (IDE hit paths) | one call | nothing, or the worker's `u32`s for a downmap | E14, F2 |
| every memo (parse, span maps, argument tree, expander, ast ids, line starts) | until recomputed or evicted | global, built once at exact length | M |

Every hir-expand query runs under `Tag::Expand` (E1); `AstIdMap::from_source` opens its own
nested task under `Tag::Parse` (`mem-format`'s request), so `ast_id_map` needs no change.
`frame::current` is opened only at the top of the five query bodies that build trees
(`parse_macro_expansion`, `macro_arg`, `decl_macro_expander`, `real_span_map`, and through them
the expanders); every function reached from hir-def or `hir::Semantics` outside a run either
takes no frame or opens its own task, never `current` (mem-db-def's rule 1).

Two places keep what they have, on purpose:
- `ModPath::from_src`/`from_tt` keep their stack `SmallVec<[Name; 8]>`: they run inside
  callbacks that call the database, which can re-enter path conversion, so a thread-local
  buffer would be borrowed twice. A path longer than 8 segments spills once.
- `MarkCache` (hygiene) is on the stack; `apply_mark` only interns (`Tag::Intern`).

## What the memo costs after

contract 4 forbids a memo grown by doubling and shrunk. Found and fixed by request (table M):
`ExpansionSpanMap` (`with_capacity(tokens)` then `shrink_to_fit`, F3), the argument tree's
`parts`, `wide` and `symbols` columns (doubling or an upper bound, then `into_boxed_slice`, F9),
`SyntaxFixupUndoInfo.original` (`ThinVec` doubling then `shrink_to_fit`, E3), `mbe`'s rules
(F7), `AstIdMap`'s entries (F6), `real_span_map`'s anchors (E8) and `line_starts` (E9,
applied). After, a miss of `parse_macro_expansion` allocates its parse (syntax's green tree)
and its span map's two columns, each once; a miss of `macro_arg` its tree's non-empty columns,
each once.

## Speed of light

The work a macro expansion must do is write its result: the parse and the span map of the
expansion, and for `macro_arg` the argument tree. Everything else (the expansion's own token
tree, matcher bindings, derive shapes, fixups) is scratch whose ideal cost is zero allocator
calls and one write of its bytes into cache-resident memory.

Take a typical `macro_rules!` call expanding to 100 tokens on one core (Apple M-series or a
recent x86: about 20 GB/s sustained single-core store bandwidth, a glibc/mimalloc
`malloc`+`free` pair about 20 ns, a bump allocation about 1 ns):

- Result bytes: span map 100 × (4 B end + 12 B span) = 1.6 KB; argument tree 100 × (8 B node
  + 8 B packed span) + parts + symbols ≈ 1.7 KB. Writing 3.3 KB at 20 GB/s is 165 ns.
- Result allocations: 2 span-map columns + up to 5 tree columns, one each: 7 × 20 ns = 140 ns.
- Bound for the allocation side of a miss: about 305 ns, plus the parser and matcher work
  itself, which is the front end's.

Today the same miss also pays: the expansion tree's 5 columns allocated and freed (5 × 20 ns),
the builder's three shrinks for each of the two trees (up to 6 reallocs, each a copy, about
6 × 30 ns), the span map's two `shrink_to_fit` reallocs (2 × 30 ns), and mbe's per-call
vectors growing from empty (mem-format-tokens measured several doublings per call over long
inputs): about 340 ns or more, over 100% above the bound.

After every request lands, what remains beyond the bound is bump allocations in the frame
(a few dozen per expansion at about 1 ns, under 30 ns, under 10% of 305 ns) and the frame's
reset (one pointer store per task). That is within 10%.

Hit paths: a warm memo hit and a warm span upmapping allocate nothing (T1 (a), T2 (a)); their
cost is a salsa memo read and a binary search over the span map's `ends` (log2 of its length,
about 7 to 10 loads for a typical expansion, each from a column that stays in cache across an
IDE request). A downmap scans `by_anchor`'s range for the span and sorts the few matches in
the worker's lent `u32`s: 0 allocations after the first, which is the floor.

State of the art for the scheme: rustc's per-`TyCtxt` arenas (`DroplessArena`) for query
temporaries and game engines' per-frame linear allocators (reset whole, never freed piecewise);
this goes one step further than rustc, whose expansion `TokenStream`s are `Lrc`-refcounted on
the global heap, by keeping expansion trees in the run's frame and only the memo on the heap.

## Tests

T1 in `crates/hir-expand/src/budget_tests.rs` (beside the owner's block 8 tests): warm span
mapping 0; an eager span map reused 0; the tokens before a `;` in the frame 0; a builtin
derive's shape and expansion 0 global; `concat!` warm 0. T2 in
`crates/hir-def/src/macro_expansion_tests/budgets.rs`: a warm `ExpansionInfo` round 0; a miss of
`parse_macro_expansion` at most salsa's run cost plus its argument, parse and span map; a miss
of a derive's `macro_arg` at most salsa's run cost plus its tree. F9 adds tt's own test (a built
tree allocates its four columns, nothing grown or shrunk).
