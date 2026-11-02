---
status: plan
summary: lane diagnostics' speed of light: for each component, the bound its work and the hardware set, the arithmetic, how the design reaches 90% of it, and what stands in the way where it does not yet.
date: 2026-10-29
---
# Lane diagnostics: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each part of lane diagnostics has a section here,
written by the agent that owns it; a sibling adds its own section below rather than rewriting
the file.

The numbers use one core of the target machines in [hardware.md](../../design/hardware.md) at
about 3.5 GHz: an L1 hit in 4 to 5 cycles, L2 in about 14, DRAM at 20 to 30 GB/s, an IPC of
about 2 for branchy code and about 4 for straight-line integer code, and about 15 cycles per
mispredicted branch. Nothing here is measured; `proof`'s budgets measure it later. None of
these passes does I/O or makes a syscall: they read what the front end has already
computed, so every bound below is compute or memory.

## `dead_code` (`diagnostics-lints-names-dead`)

`crates/blaze-diagnostics/src/lints/names/dead/`: rustc's `rustc_passes/src/dead.rs`. It
numbers the crate's definitions (`Defs`), reads their lint levels (`level`), works out what is
reachable from outside (`reach`), seeds the worklist (`seed`), walks each live definition's
signature and body to mark what it names (`mark`), and reports what is left (`report`).

**The reference crate.** About 100,000 lines, which gives:

- about 800,000 syntax nodes, at about 8 per line;
- about 500,000 expression, pattern and type nodes in bodies, at about 5 per line;
- about 120,000 paths in bodies, plus about 60,000 method calls and field accesses;
- about 10,000 definitions, counting fields and variants;
- about 3,000 bodies.

Only the ratios matter: each cost below scales with one of these counts.

**The state of the art** is rustc's own pass. `MarkSymbolVisitor` walks HIR once per live
body. Each path in HIR already carries its `Res` from AST lowering, and type-relative paths,
methods and fields are one lookup in `TypeckResults`. Lint levels come from
`ShallowLintLevelMap`, one per owner. It runs on one thread. The bound below is that design
with nothing extra, which is also the most rustc can do.

### Bounds and where the pass stands

| Component | Work | Bound | Now |
| --- | --- | --- | --- |
| `Defs` | one visit per definition | about 0.6 ms | about 7 ms (estimate by count; was about 10 ms) |
| `level` | one lookup per owner | about 0.15 ms | about 1 ms (estimate by count; was about 1.4 ms) |
| `reach` | one visit per definition and per type in a public signature | the same as `items` | computed once, shared with `items`; then one bit per definition |
| `seed` | one attribute-bit test per definition | about 0.3 ms | at the bound: `rustc_main` and `rustc_canonical_symbol` are `AttrFlags` bits |
| `mark` (bodies) | one visit per body node and one read per resolution | about 5.6 ms | about 9 ms (estimate by count; was about 20 ms, before that about 150 ms) |
| `report` | one bit test per definition, text only for what fires | under 0.1 ms | at the bound, except that it runs when the lint can't fire |

The arithmetic behind each row follows.

**`Defs`: the crate's definitions, numbered.** The work is the definitions themselves:
10,000 of them at about 200 cycles each (a few memoized queries and one hash insert) is
2 million cycles, about 0.6 ms. Their ids, names and kinds are about 64 bytes each, 0.64 MB
read, well under a millisecond at DRAM bandwidth.

The pass is not there yet. `Defs::of` finds definitions by walking all the syntax, because
items in blocks and anonymous constants appear only there. The walk is over the flat syntax
view now, so no cursor is made: each of 800,000 nodes costs a children step and a kind
dispatch, about 10 cycles, and each of the 400,000 attribute owners `enabled`'s scan of its
children for a `cfg`, about 40 cycles. That is 8 + 16 = 24 million cycles, about 7 ms
(estimate by count), 12 times the bound. The flat tree's attribute bit (queue 00808) takes
the scans off owners without attributes, about 3 ms.

What removes it: enumerate items from hir-def's `ItemTree`, which name resolution has
already lowered, including each block's tree. Take anonymous constants and closures from the
expression store, which `mark` already walks. The syntax walk is then left with the
definitions' own nodes. This is a rewrite of `defs.rs` that this lane can do; I did not do it
blind, without a build. `Defs::in_anon` already skips the anonymous-constant work in a crate
that has none.

**`level`: lint levels.** The bound is rustc's: one map per owner and one lookup per query.
That is 10,000 definitions plus the closures and anonymous constants `mark` asks about, at
about 50 cycles each, about 0.15 ms.

Now: `hir_lint_levels::Levels::set_at` memoizes every node on the chain it walks, so a query
walks up only to the nearest node seen before, usually its item's or its sibling's parent:
one or two steps and probes, about 150 cycles. Each node is pushed once, with its
attributes read, about 100 cycles. For about 15,000 queries over about 30,000 distinct nodes
that is 2.3 + 3 million cycles, about 1 ms (estimate by count). What removes it: a lint-level
column keyed by
`AttrDefId` and computed once per crate, as `ShallowLintLevelMap` is. That is for the lane
that owns `Cx`'s levels; this pass would read it unchanged through `Levels::at`.

**`reach`: what is reachable from outside.** The bound is rustc's
`effective_visibilities`, computed once per crate for every pass that reads it.

`names::effective` computes it once: `names::after_abort` builds one `Effective` and passes
it to `dead` and to `items` (`check_private_in_public` and the late item lints). Each
definition is numbered once into a hash map, and its visibilities, its own visibility and its
private one are columns over that numbering. The fixpoints re-walk a definition only when it
rises, so the cost is one walk of each interface per rise; a rise moves a level up the module
tree, so a definition rises at most once per level per enclosing module, in practice once or
twice.
`dead`'s `Reach` then reads it into a bitset over its own numbering: one hash lookup per
definition, once per crate, and after that a bit test per question.

**`seed`: the worklist's first entries.** The bound is one memoized `AttrFlags` read per
definition at about 100 cycles: 10,000 of them is 1 million cycles, about 0.3 ms.

Now: at the bound. `rustc_main` and `rustc_canonical_symbol` are the `AttrFlags` bits
`RUSTC_MAIN` and `RUSTC_CANONICAL_SYMBOL`, and `seed.rs` tests them directly (the canonical
symbol on the flags it has already read for the definition), which is what `has_word` does
for these two words, without the hash of the name.

**`mark`: what bodies name.** This is the pass's main work, and the component this change
rewrote.

Before, bodies were walked over syntax and every name resolved through `hir::Semantics`. Each
`resolve_path`, `resolve_method_call`, `resolve_field` or `type_of_*` call rebuilt a
`SourceAnalyzer`:

- `find_container`: about 10 ancestors and dyn-map lookups, about 500 cycles;
- three memo fetches (`Body::with_source_map`, `ExprScopes::of`, `InferenceResult::of`),
  about 300 cycles;
- `scope_for`: about 10 ancestors, each hashed into the source map, about 600 cycles;
- a fresh `Resolver`: an allocation plus about 15 scopes, about 500 cycles;
- for a path, an `ExprCollector` and `lower_path` run again, about 1,000 cycles.

That is 2,000 to 3,000 cycles per call. 200,000 calls at about 2,500 cycles is 500 million
cycles, about 140 ms. A rowan cursor for each of about 600,000 body nodes at about 45 cycles
adds 27 million cycles, about 8 ms. The total was about 150 ms.

The bound is one visit per store node and one indexed read per resolution:

- **Visiting the store.** 500,000 nodes at about 35 cycles each: a column decode of about 10
  cycles, the kind dispatch with its share of a mispredict at about 15, and the visit at about
  10. That is 17.5 million cycles, about 5 ms. It is compute-bound: the columns are about
  16 bytes a node, 8 MB, which DRAM delivers in 0.3 ms.
- **Reading resolutions.** If each resolution were one load from a column indexed by
  `ExprOrPatId`, 180,000 of them at about 10 cycles would be 2 million cycles, about 0.6 ms.

So the bound is about 5.6 ms on one core.

Now (`mark/body.rs`), each body is walked over hir-def's `ExpressionStore` with its
`InferenceResult` and one `Resolver` kept at the walk's scope:

- **Visiting the store** costs what the bound says, about 5 ms.
- **Methods, fields, associated items and variants** are read from what inference recorded:
  hash-map lookups at about 50 cycles each. For 60,000 of them that is 3 million cycles,
  about 0.9 ms.
- **Value paths** (functions, statics, constants, unit structs, locals) are read from the
  columns inference records (queue 00806): `InferenceResult::value_resolution` for a path
  resolved whole, `qualifier_resolution` for the type a type-relative path (`S::new`,
  `T::f`, `Alias::A`) goes through. A one-segment or type-relative path, about 80% of
  120,000, is two column reads at about 10 cycles, about 0.3 ms. A qualified path resolved
  whole (`m::f`, about 24,000) still asks the resolver for its qualifier's type
  (`update_to_inner_scope` and `resolve_path_in_type_ns`, about 400 cycles): about 2.7 ms.
  Before, every value path paid both lookups, about 13.4 ms.
- **Per-body memo fetches** (3,000 of them) round to nothing.

That totals about 9 ms (estimate by count; was about 20 ms), 62% of the bound.

`recorded_value_paths_are_what_the_resolver_finds` (in `mark/body.rs`) checks that both
columns match the resolver at each path's own scope. The walk kept its resolver at the
enclosing block's scope, so where a `let` earlier in the block shadows an item
(`let f = 1; f`), the recorded local is what rustc's `qpath_res` gives. There the walk no
longer marks the item live.

What stands in the way, and what removes it:

- **A qualified path's module prefix is not recorded.** That is the 2.7 ms above (queue
  00528, module-prefix segments). Type-ref paths in bodies are queue 00833 (db-ty).
- **Recorded resolutions are hash maps, not columns.** This costs about 0.5 ms, under 10%
  once the column above lands. A dense column for methods and fields would close it.
- **The pass runs on one core.** rustc's does too, so this is not behind the state of the
  art, but the hardware bound is lower. A body's uses (the definitions it names, and whether
  each is named inside an `#[allow(dead_code)]` closure or anonymous constant) don't depend
  on liveness. Only which bodies are walked does. Split `mark` into a memoized per-body
  "uses" query, which is parallel and incremental, and a serial traversal of the definition
  graph. Each edge costs a few cycles, so about 200,000 edges is about 0.2 ms. On 16 cores the
  critical path is then the longest body plus the traversal, about 0.6 ms in total. This is
  the long-term design; it needs the column above first, so that a body's query reads no
  resolver state.

Signatures (types, generics, discriminants, constants in types) are still walked over syntax
through `Semantics`. Their cost scales with signature nodes, not body nodes. On the reference
crate that is about 10,000 definitions at a few resolutions each, about 3 ms on the old
per-call cost. Moving them onto hir-def's `TypeRef` store is the same rewrite as for bodies,
and is next once bodies are proven.

**`report`: the diagnostics.** One bit test per definition (10,000 at about 5 cycles) plus
text only for what fires; this is at the bound. Under `-A dead_code`, or for the
allow-by-default `dead_code_pub_in_binary`, the bound is zero, but the pass still runs. What
removes it: `cx.lint_may_fire`, the imports sibling's request 8. With it, the whole pass is
skipped where nothing can fire.

### Allocation

`tools/v2/hot_lint.py` finds nothing in `lints/names/dead/`. What it flagged was fixed where
a borrow or a move would do:

- the syntax walk takes owned children;
- `Levels::at` takes `InFile<&SyntaxNode>`;
- a block's modifiers are read as tokens, with no cast;
- `reach` reads the shared `names::effective` table, and keeps no walk of its own;
- `Bits` and `Live` no longer derive `Clone`. The one copy rustc's pass needs, the liveness
  before deferred seeding, is `Live::snapshot`, once per crate.

What is left is justified on its line, and is one of:

- the crate's tables and worklists, allocated once per crate;
- `SmallVec`s inline up to four;
- a diagnostic's text, built only when the lint fires;
- rowan's by-value `cast` in the signature walk, through `defs::cast`, which tests the kind
  first. A clone is a reference count there, not an allocation.

The allocation-budget tests are in `lints/names/dead/tests.rs`: a cached level or attribute
answer, a lookup in the numbering, and a bitset test and insert, each at zero. They wait on
the crate's dev-dependency on `blaze-alloc-count` (requested).

Not yet at zero: a level asked at a node that is not memoized walks ancestors, and rowan
allocates each ancestor's cursor. The per-owner level column named under `level` makes that
a lookup too.

### Memory

[memory.md](../../design/memory.md) settles allocators by lifetime, with zero
general-purpose allocations in the warm loop. A crate check is one task, and everything the
pass builds is dropped when it returns, except its `Vec<Found>`.

What the pass holds, per 100,000 lines:

| Structure | Size |
| --- | --- |
| `Def` column | 10,000 × about 70 B ≈ 700 KB |
| `Id` to `DefIdx` map | 10,000 × about 20 B, at about 0.9 load ≈ 220 KB |
| CSR | about 80 KB |
| Level and attribute cells | about 250 KB |
| Level memo maps | one entry per node that has attributes, about 100 KB |
| Four bitsets | 5 KB |
| Worklists | at most about 250 KB |

That is about 1.6 MB a run. On the target machines it fits in L2 plus a slice of L3, so the
bounds above, which assume cache-resident tables, hold.

The design puts the run in a `frame::task`, with each column and worklist a
`Vec<T, &Frame>`. The run's peak becomes a bump pointer reset at the end. No general
allocation happens when the frame is warm, and the bytes are counted under the lint's tag.
The output, a few diagnostics, is the one allocation left, and only where a lint fires.

Until then, the worklists are reserved from the numbering's counts, so they don't grow.
Each run now makes about 150 to 200 calls of its own into the global allocator, which are
the vectors and maps the frame will hold.

What stands in the way:

- `blaze-diagnostics` doesn't depend on `blaze-mem` or enable `allocator_api` (requested of
  the crate's owner).
- Whether the two hash maps take hashbrown with an allocator parameter, which tag a lint
  pass counts under, and whether the output belongs to the check's request arena (asked of
  mem).
- rowan allocates a cursor for each syntax node walked, and `Semantics` allocates per call.
  The store walk removes both from bodies. For signatures, and for `Defs` until it reads the
  `ItemTree`, they remain, and they are the syntax lane's to remove.

### The whole pass

| | One core | 16 cores |
| --- | --- | --- |
| Before this change | about 165 ms | the same |
| Now | about 32 ms | the same |
| With the value-path column | about 18 ms | the same |
| With `Defs` from the `ItemTree` as well | about 8 ms | the same |
| The bound | about 7 ms | about 1 ms, with per-body "uses" queries |

On one core, `Defs` from the `ItemTree` plus the value-path column put the pass at about 90%
of its bound. rustc's pass is also serial, so it is a peer of the one-core bound; the 16-core
bound is the per-body query design above.

## Names: imports, stability and item lints (`diagnostics-lints-names`)

`crates/blaze-diagnostics/src/lints/names/`, apart from `dead/` (its own section above):
`imports` (rustc_resolve's lints), `stability` (`deprecated`, E0658), `items` (the item
lints before the abort, `check_private_in_public`, the late item passes,
`clashing_extern_declarations`), and `effective` (`effective_visibilities`, shared with
`dead`). It uses the same reference crate as `dead_code`: 800,000 syntax nodes, 180,000 paths,
method calls and field accesses in bodies, 10,000 definitions, 3,000 bodies, and about 2,000
`use` trees.

**The state of the art** is rustc's own passes:

- resolve marks each import used as it resolves (`record_use`), and `check_unused` then reads
  only the imports;
- `Checker` (stability) and the late lint pass walk HIR once, with each path's `Res` already
  recorded on it;
- `effective_visibilities` is computed once;
- `lints_that_dont_need_to_run` drops every pass whose lints are all allowed.

The bounds below are that design with nothing extra.

### Bounds and where the passes stand

| Component | Work | Bound | Now |
| --- | --- | --- | --- |
| skip of allowed lints | one AND of a `LintSet` per pass | under 1 µs | at the bound: every pass gates on `cx.any_may_fire` / `lint_may_fire`; `stability` runs anyway for E0658 |
| syntax walk | one pass over the crate's nodes, shared | about 0.5 ms | `imports` and `stability` read `cx.walk()`; `stability` scans its `kinds` column (about 0.5 ms) and reads only the nodes of the kinds it checks (about 2 ms) |
| item walk | one pass over the items, shared | about 0.3 ms | at the bound: `cx.items()`, made once for both sides of the abort |
| `imports` | one bit per import, then one visit per `use` tree | about 0.1 ms | about 150 ms: every path is resolved again; about 25 ms once `used.rs` reads `used_imports_with_traits` (estimate by count) |
| `stability` | one table probe per resolved use | about 2.6 ms | about 150 ms: every path is resolved again (value columns exist; type paths 00833, prefixes 00528, and a store walk here) |
| `items`, after the abort | 17 checks at each of 10,000 items | about 2.4 ms | near the bound, plus on-demand path resolutions in five passes |
| `effective` | one walk of each public interface per rise | the `dead` section's `reach` row | computed once, shared |

The arithmetic behind each row follows.

**Skipping allowed lints.** `cx.any_may_fire(&GIVES)` is one AND per word of a bitset built
once per check. `imports` and `dead` test their whole set before any other work, and each
sub-pass of `items` and `imports` tests its own. So under `--cap-lints allow` (every
dependency cargo builds) `imports`, with its 150 ms of resolving paths again, and `dead` cost
nothing. `stability` can't be skipped that way: E0658 is an error, not a lint, and std is a
`staged_api` crate every crate uses, so its walk and resolutions run in every build. Its lints
are skipped one by one (`deprecated` and `deprecated_in_future` before any text is made). E0446
and the `#[macro_escape]` warning are not lints either.

**The syntax walk.** One read of a node's kind from the walk's pre-order columns is about 2
cycles; 800,000 nodes is 1.6 million cycles, about 0.5 ms. `imports` and `stability` read
`cx.walk()`, the early lint pass's walk, which `lints::syntax` builds once. `stability/walk.rs`
scans each tree's `kinds` column, about 0.5 ms of its own, and touches a node only when its
kind is one it checks (items, attributes, `let`s, paths, `use` trees, method calls, fields,
parameters, generic parameters and arguments): a few hundred thousand nodes, at about 30
cycles each for the node's range and cast, about 2 ms. An expansion's or a module file's tree
is walked where it is reached, so the owners and modules are numbered as rustc's walk numbers
them. Inside a body it keeps typeck's order (a receiver before its method; a `let`'s type,
initializer, pattern, then the rest, found in one pass over the `let`'s children). Whether a
path is in a body is found only where a check needs it (type-relative paths and associated
type constraints), by walking up to the nearest item or expression. These are estimates, not
measured: the port was made without a build.

**`imports`.** rustc's bound: resolve already recorded the uses, so the lint is one bit test
per binding plus one visit of each `use` tree to group what is unused. 2,000 trees at about
200 cycles is 0.4 million cycles, about 0.1 ms. Now, without that record, `imports/used.rs`
resolves every path again. Its own prefix lookups are memoized in one `HashTable`, at about
100 cycles a hit: 180,000 paths cost 18 million cycles, about 5 ms. Each path is also checked
against `sema.resolve_path`, whose `analyze_impl` (`find_container`, a `SourceAnalyzer` for
the body, a source-map lookup, a resolver for the scope) costs about 3,000 cycles: 540
million cycles, about 150 ms. That is three orders of magnitude over the bound.

The front end now has imports' request 1: `hir_ty::used_imports_with_traits(krate)` is
rustc's `import_use_map`, `glob_map` and `#[macro_use]` uses, with the traits inference picked.
Its walk (`hir_def::nameres::uses::crate_uses`) resolves each store's paths through one
`Resolver` per scope, item-scope lookups at about 500 cycles a path: 180,000 paths is 90
million cycles, about 25 ms (estimate by count), memoized per crate by salsa. `used.rs` does
not read it yet. The test
`lints::names::imports::used::tests::what_the_walk_finds_used_is_what_the_front_end_records`
compares the two; once a build shows they agree, `used` and `typeck` come from the query
and the per-path `sema.resolve_path` goes, taking `imports` to about 25 ms. What is left
after that is the trait-import candidates of method calls and fields (`lookup_trait_item`,
one `sema.scope` per call, which the query doesn't give), and the walk itself. The bound
needs nameres and inference to record uses as they resolve: queue 00806 (path resolutions
recorded by inference and type lowering) and 00528.

**`stability`.** rustc's bound: for each use, one probe of the deprecation and stability
index, which `stability/lookup.rs` already is: a `HashTable` hashed once, its 8-byte slots in
`u32`-numbered arenas, at about 50 cycles a probe. 180,000 uses cost 9 million cycles, about
2.6 ms. Now each use's resolution is a `sema.resolve_path`, or a `resolve_method_call` for
method calls and fields, at about 3,000 cycles: about 150 ms, plus the 2.5 ms walk above. The
same resolution table removes it. Its value half exists (queue 00806:
`InferenceResult::value_resolution` and `qualifier_resolution`, which `mark` reads). For
`stability` the rest is type paths (queue 00833) and module prefixes (00528). There is also a
change in this pass: `stability/walk.rs` walks syntax, so it would walk each body's store, as
`mark` does, to read the columns by `ExprOrPatId`. That rewrite needs a build.
Until then a memo shared with `imports` saves little: `imports` resolves the last segment
per namespace and `stability` the path as written, so they share only qualifiers. A name pre-filter (skip a path whose last segment names no
deprecated or unstable definition) is not sound: `use old as new` renames, and std has
deprecated items in every crate's scope.

**`items` after the abort.** The bound is the 17 checks of `BuiltinCombinedLateLintModPass`
at each item, most a `SyntaxKind` test or an attribute bit at about 50 cycles: 10,000 items
times 17 is 8.5 million cycles, about 2.4 ms. The walk is shared, the crate's features and
lang items are read once (`items::Krate`), and text is made only for what fires. It is near
the bound except where five passes resolve a path on demand (`bounds::drop`, `c_void`,
`lifetime_syntax`, `naming`'s path patterns, `privacy::visit`), each at about 3,000 cycles.
Those paths are few (bounds, return types, patterns of consts), and the same resolution table
removes their cost.

**`effective`.** See `reach` in the `dead_code` section: computed once in
`names::after_abort` and passed to `dead` and to `items`.

### The whole pass

Bound: about 6 ms for the 100,000-line crate on one core: the shared walks about 0.8 ms,
`imports` 0.1 ms, `stability` 2.6 ms, `items` 2.4 ms. Now: about 310 ms, nearly all of it
paths resolved again through `Semantics`, once by `imports` and once by `stability`.

What stands in the way of the 90% mark is mostly in the front end: which imports each
resolution went through (imports' request 1, now `used_imports_with_traits`, which `used.rs`
reads once its comparison test passes), and one table of path resolutions recorded as
lowering and inference make them (queue 00806 and 00528). With both, `imports` and `stability` drop to their bounds without
changing their rules. `stability`'s walk now reads `cx.walk()` (about 10 ms to about 2.5 ms,
estimated); what is left of it is its own scan of the `kinds` column and the nodes it checks.

None of these passes does I/O or a syscall, and none is parallel. Modules are independent for
`items` and `stability`, so the passes could split across cores by module. At a 6 ms bound
on one core, with the driver already running crates in parallel, that is not where the time is.

## The driver, dedup and closing lines (`diagnostics`, the parent)

`check.rs`, `model.rs`, `emit/` and `api.rs`. The parts that run on every check of a crate
that has nothing to report are `check_crate_with`'s glue: the task frame, the phase sort, the
partition of stashed warnings, `finish` and `summary`. Everything else here runs per
diagnostic, so it is the edge (principles.md: formatting happens there).

| Component | Work | Bound | Now |
| --- | --- | --- | --- |
| A check's task | one `frame::task`, one frame reset | under 1 µs | at the bound |
| Phase sort | radix sort on a `u8` key over what was found | about 2 cycles per diagnostic | at the bound (`blaze_ids::radix`) |
| `finish` (dedup, once-notes) | one 128-bit hash per diagnostic and per once-note | about 200 ns per diagnostic of about 200 bytes | at the bound; no copy kept |
| `summary` | one pass counting levels, codes sorted only when an error has one | zero allocations with nothing to say | at the bound (budget test) |
| Rendering (`emit`) | annotate-snippets per diagnostic | rustc's own emitter | the same library and version as rustc |

**The state of the art.** rustc's `DiagCtxtInner::emit_diagnostic` dedups with a set of
`Hash128` per diagnostic emitted (a StableHasher, SipHash-128). Ours is the same design with a
faster hash: BLAKE3 cut to 128 bits, the hash principles.md names, whose SIMD paths run at
several GB/s against SipHash's about 1 to 2. Before this pass the set held a clone of every
diagnostic, and the closing count a second clone. It now holds 16 bytes per diagnostic and
counts by `u32` index into what is written.

**Memory.** A check is one `blaze_mem::frame::task(Tag::Diagnostics, ..)`. `Cx::frame()`
gives it to every pass, so their crate-sized tables are `Vec<T, &Frame>` and are freed in one
reset when the check returns. Only the `Vec<Diag>` it returns outlives the task, and on a crate
with nothing to report that `Vec` is empty and unallocated.

**Budget test.** `check.rs`, `a_crate_with_nothing_to_say_costs_the_driver_no_allocation`:
`written` with nothing found makes zero allocations.

**What stands in the way.**
- `Tag::Diagnostics` is requested of mem (`v2/requests/diagnostics.md` 9).
- The passes the driver calls are the rest of the cost. Their sections follow, one per part:
  `diagnostics-speed-{checks,lower,lints,bodies}`.
- Every check walks the whole crate. The edit bound ("recomputing only what changed") needs
  each pass's results per item, as queries, so an edit rechecks only the items it touched. That
  is the next step after the per-pass speed work. It needs the passes to read per-item inputs,
  which they mostly do (the store's bodies and the item tree).

## Lowering and printing (`diagnostics-speed-lower`)

`crates/blaze-diagnostics/src/lower/` turns each `hir::AnyDiagnostic` into rustc's
diagnostic. `print/` prints types and paths as rustc does. Most lowerings run only when
something is wrong, which is the edge. Five things run on a crate with nothing to report, or
once per suggestion over the whole crate:

- `lower/parse.rs` (every file);
- the style and `unused_*` lints lowered from hir;
- the typo and import-candidate helpers of `lower/unresolved_ident.rs`;
- `lower/unresolved_import.rs`'s `Resolver`;
- `print::Paths`, rustc's `trimmed_def_paths` and `visible_parent_map`.

**The reference crate** is `dead_code`'s, about 100,000 lines and 800,000 syntax nodes,
taken as about 300 files and 300 modules. Its direct dependencies are taken to add about
90,000 declarations to the crate's 10,000, so about 100,000 (`D`). About 50,000 of those are
public children reached from the dependencies' roots (`T`). These two numbers are
assumptions; only the ratios matter.

| Component | Work | Bound | Now |
| --- | --- | --- | --- |
| `parse`, clean crate | one memoized read per file | about 10 µs | at the bound, a dependency's outline included (`parse_errors_with_bodies`) |
| Style lints from hir (`incorrect_case`) | one case check of the name's text | about 50 cycles per name | at the bound; one bit test when the lint is allowed everywhere |
| `elided_lifetimes_in_paths` | nothing, being allow-by-default | one bit test per report | at the bound |
| `Paths` (printing) | one listing of the public children, two walks over it | about 2 ms, once per check that prints a path | at the bound; nothing on a clean crate |
| `Decls` (imports) | one pass over `D` declarations, then one probe per name | about 1.7 ms once, then about 50 ns per lookup | at the bound |
| `typo` | names in scope, an edit distance each, one span | rustc's `lookup_typo_candidate` | at the bound |

**`parse`.** rustc lexes and parses each file once and stores its errors. Before this pass the
clean path called `errors_with_bodies()` on every file. On a file parsed in full that runs
`validation::validate` over the whole tree each time: 800,000 nodes at about 30 cycles is
24 million cycles, about 7 ms per check. It also allocated two `Vec`s per module
(`checks::modules`' children and `path_to_root`). Now a crate whose files all parse is one pass
over the def map's module table, reading the memoized `parse_errors` of each file: 300 salsa
memo hits at about 100 cycles, about 10 µs, with no allocation. The ordered walk of rustc's
module order runs only when some file has an error.

**Style lints.** hir finds names by rust-analyzer's rules and rustc's rules drop some of them.
Before, every name was turned into a syntax node and a span before rustc's check ran. The check
also allocated a `Vec` per character (`to_titlecase`), plus a `String` for the kind. Now
rustc's check of the text runs first, and the case mapping is an iterator. `lint_may_fire`, one
bit test, skips the lowering where the lint is allowed everywhere (`--cap-lints allow` on a
dependency, or `#![allow(..)]` at the crate root). `elided_lifetimes_in_paths` is allowed by
default and hir reports every elided path, so the same test turns about a thousand full
`Diag`s per crate, built and then dropped, into a thousand bit tests. `unused_must_use`,
`borrowck_error`'s `unused_mut` and `missing_unsafe`'s `unsafe_op_in_unsafe_fn` get the same
test. So does `deprecated_safe_2024` for a call whose attribute says what to audit; without one,
the call is E0133 at any level. Under `--cap-lints allow` they cost a
dependency nothing.

**`Paths`.** rustc's `trimmed_def_paths` builds one map per crate session by walking the
visible children of every crate. `visible_parent_map` is a second breadth-first walk. Before,
blaze listed every module's children twice, once per table, each time into a fresh `Vec`. Now
`Tree` lists the public children once into frame columns (`T` rows, about 50,000). The
trimmed-name walk (depth first, a `u64` bitset for what it has seen) and the visible-parent walk
(breadth first, a queue column with a head index) read those rows. At about 150 cycles per row
(a hash insert and a `#[doc(hidden)]` check), that is 7.5 million cycles, about 2 ms, which is
rustc's one map per crate. It is built only when a path is first printed, so it costs nothing
on a clean crate. Every lookup afterwards is one probe: `trimmed`, `visible_parent`,
`extern_crate`, and `reexport_name`, which also scans one node's rows.

**`Decls`, the import candidates.** rustc's `lookup_import_candidates` walks every module of
every crate for each name it looks up: `O(D)`, about 1.7 ms, per unresolved name. blaze did the
same, collecting each module's declarations into a `Vec` on the way. Now `Decls` walks the
`D` declarations once per check, in the frame, at about 60 cycles each (one `FxHash` entry on
the interned `Symbol`, one push). It then groups them by name with a counting placement: two
sequential passes, no comparison sort. A lookup is `Symbol::find`, which interns nothing, plus
one probe, and returns a slice, with zero allocations. For `k` unresolved names that is
`O(D + k)` against rustc's `O(k·D)`, which beats rustc from the second name on.
`unresolved_import`'s `module_candidate` uses `Decls` too, instead of every module's name.
`checks/names/paths.rs` is to use it for `named` and `variant_enums`
(`v2/requests/diagnostics-speed-lower.md` 4).

**`typo`.** rustc's `lookup_typo_candidate` collects the names in scope of the namespace,
sorts them and takes `find_best_match_for_name`, an edit distance per candidate. The shared
`typo` (`pub(crate)`, taking `Names` and `PathSource`) does the same, matching it. Before, the
value-namespace copy also computed the definition's head span for every candidate. That is a
`to_string` of the item's whole text, so it cost the total length of every item in scope. Now
names stay `&'static str` from the interner, and one span is computed, for the candidate
chosen. `unresolved_import`'s `Resolver` asks Semantics for its module once (`current`, in a
`OnceCell`), not once per path segment.

**Budget tests.** In `tests/bodies/speed.rs`:

- `a_clean_crate_parses_in_constant_allocations`: `parse::parsed` of 4 clean files and of 32
  makes the same number of allocations, at most 2.
- `path_and_import_lookups_allocate_nothing`: once the tables are built, `Decls::named`,
  `trimmed`, `visible_parent` and `reexport_name` make zero allocations.

**What stands in the way.**
- `cx.rs` must hold `Paths<'a>` (`v2/requests/diagnostics-speed-lower.md` 1). The crate does not
  compile without it.
- `incorrect_case` reads hir's kind through its `Display` into a stack buffer until hir exports
  `IdentType` (request 2).
- `checks/names/paths.rs` keeps its own copies of `typo` and the import candidates, scanning
  every module per lookup, until it uses the shared ones (request 4).
- `typo` still copies the names in scope into a `Vec` per diagnostic, which is rustc's design.
  Below that, the bound is a per-module index of names by length for the edit-distance cutoff.
  That only pays off when many typos are reported in one scope.

## Bodies (`diagnostics-speed-bodies`)

`crates/blaze-diagnostics/src/lints/bodies.rs` and `lints/bodies/`: what rustc lints in
bodies. Before the abort, there are six passes per body: typeck's `unreachable_code`, match
checking, `unused_unsafe`, `unconditional_recursion`, liveness and the known-panics lint.
After the abort, there are rustc's 31 late lint passes of expressions, statements and patterns
(`BuiltinCombinedLateLintModPass`).

**The reference crate** is the one `dead_code` uses: 100,000 lines, 500,000 body nodes and
3,000 bodies, about 170 nodes each. Of the nodes, about 120,000 are paths, about 60,000
method calls and about 60,000 calls. The MIR is about two statements per body node, so about
1 million statements. Cycles are at 3 GHz.

**The state of the art.** rustc's late lints are one HIR walk with every builtin pass in one
combined pass. Each pass's `check_expr` is inlined at every node and tests the node's kind
itself. Only the non-builtin passes are dropped by `lints_that_dont_need_to_run`; of the
builtin ones, `IfLetRescope` checks it itself. Liveness is `check_liveness` on MIR
(`rustc_mir_transform/src/liveness.rs`): one dataflow per body over MIR that borrow checking
builds anyway.

### Bounds and where the passes stand

None of the "now" figures below are timed: this lane runs no build. Each one counts the
operations in the code, using the costs of the `dead_code` section.

| Component | Work | Bound | Now | Was |
| --- | --- | --- | --- | --- |
| The late walk | one decode and one tag dispatch per node | about 2.5 ms | at the bound | about 130 ms |
| The late passes at the nodes they look at | one shared callee per call, one compare per pass | about 6 ms | about 15 ms | about 19 ms |
| A fully allowed pass | none | 0 | 0 | as if it fired |
| Liveness | one dataflow over the MIR | about 7 ms | about 10.5 ms (estimate by count) | about 12 ms |
| `unused_unsafe` | one tag scan of a body without `unsafe` | about 1 ms | about 2 ms (estimate by count): the pre-scan reads one kind byte per expression and decodes only blocks | about 3 ms |

**The late walk.** The bound is one visit per node: a decode from the store's columns and a
tag dispatch, about 15 cycles. 500,000 nodes is 7.5 million cycles, about 2.5 ms.

The walk was 31 passes behind `Box<dyn LatePass>`, every pass called at every node, each one
decoding the node again to test its kind. At about 25 cycles a call, that was 500,000 × 31 ×
25 = 390 million cycles, about 130 ms. Now each node is decoded once. Its tag indexes a table,
built once per crate, of the passes that look at that kind (`LatePass::EXPRS`, `PATS`,
`HOOKS`). Each pass is a variant of the `Pass` enum, called by `match`. A literal reaches 1
pass, a path 1, and a block, `let` or field access 0 to 4. That is what rustc's combined pass
does, minus the kind test of every pass at every node, so it matches or beats rustc's walk.

**The late passes at the nodes they look at.** 12 passes look at a call and 9 at a method
call. Each one decodes the node again, reads a type from `InferenceResult` and compares the
callee or the method with what it lints. That is about 45 cycles per pass:
60,000 × 12 × 45 + 60,000 × 9 × 45 is about 57 million cycles, about 19 ms. Resolving a
path's value goes through one per-body cache (`BodyCx::path_value`), so the passes that ask
about one callee pay for the scope resolver once.

Some per-node work now comes after the cheapest test that can rule a node out:
- `MutableTransmutes` tests the path's `FnDef` before resolving the path.
- `UnusedAllocation` reads the adjustments before resolving the path.
- `noop_method_call` resolves the impl before reading the source.
- `map_unit_fn` resolves the method before walking ancestors.
- `CheckTransmutes` builds its closure and const map only for a body with a transmute in it.
- `IfLetRescope` records parents during the walk, with no prescan of the body.
- `DropForgetUseless` records only the match arms that are calls.

With those, "now" is about 15 ms. The bound is a fact computed once per call node and shared
by the passes: the callee's `FunctionId` and diagnostic item, then a compare per pass. That
is 120,000 × (100 + 10 × 5) cycles, about 6 ms.

**A fully allowed pass.** Each pass declares its lints as a `LintSet` made at compile time
(`lint_set`, through `LintId::named`, so a misspelled name fails to build).
`Passes::new` leaves a pass out of every table when `Cx::any_may_fire` says none of its lints
can fire. `before_abort` does the same for its six passes (`Runs`). The test is one AND per
word, once per crate. rustc runs its builtin passes even when they are allowed, so on a crate
with `#![allow(...)]` this beats rustc. `DanglingPointers` checks each of its two halves
against its own lint. A crate that allows every body lint returns before it looks up a body.

**Liveness and the MIR lints.** The bound is one dataflow per body over MIR that is built
once. Borrow checking builds that MIR, and `BodyCx::mir` and `runtime_mir` fetch it once per
body for every pass. 1 million statements at about 20 cycles is about 7 ms. `is_naked` is
now the `AttrFlags::NAKED` bit, rustc's attribute test, where it scanned every statement
(1 million at about 4 cycles, about 1 ms). `source::skipped` tests
`AttrFlags::AUTOMATICALLY_DERIVED` before it reads the impl's source, where it read the
source of the impl of every trait-impl method (about 3,000 at about 600 cycles, about
0.5 ms). With `cfg_attr(.., automatically_derived)` it now follows rustc, which expands the
`cfg_attr` first. The guard scan still reads the statements again. Liveness also builds an
`InferCtxt` per body, about 5 µs, paid only by bodies with a variable to check. Together that
is about 10.5 ms (estimate by count).

The fastest technique left is to share one `InferCtxt` of `TypingMode::non_body_analysis`
across the crate's bodies. That can't be done inside this lane's paths (see the blockers
below).

**Preorders and buffers.** `BodyCx::exprs` is built only when a pass asks for it: liveness,
the pointer lints, `invalid_from_utf8` and literals. It is built in `Scratch` buffers handed
from body to body, and the `path_value` cache uses the same scratch. The crate grows them to
its largest body once. The taint of a body, and the macro-expansion errors behind it, is
worked out when a pass first asks, no longer for every body up front.

**Budget tests.** In `lints/bodies/tests.rs`:

- `dispatch_tables`: building `Passes::new` makes 0 allocations.
- `preorder_reuses_scratch`: once the buffers are grown, the preorders of 8 bodies make 0
  allocations.
- `late_passes_per_crate`: `after_abort` makes no more allocations at 64 bodies than at 4.

**What blocks the last 10%.**
- Request 1 is done: `ExpressionStore::kinds_present`, `has_closures` and `expr_kind`.
  Liveness's closure loop reads `has_closures`, and `unused_unsafe`'s pre-scan reads the kind
  bytes. Not yet done: skipping the late walk for passes whose kinds a body lacks (a mask per
  pass ANDed with `kinds_present`). That touches the dispatch tables and needs a build.
- Request 2 is done: `#[naked]` and `#[automatically_derived]` are `AttrFlags` bits, which
  `is_naked` and `source::skipped` read.
- `body_owners` (each body's file rank and offset, then a radix sort) runs in both
  `before_abort` and `after_abort`. The parent could compute it once. Request 3.
- A shared per-call callee fact needs the passes to take it from the walk. That is a rewrite
  of 21 passes' `check_expr`; no build could verify it in this lane.

## The checks hir does not make (`diagnostics-speed-checks`)

`crates/blaze-diagnostics/src/checks.rs` and `checks/`: feature gates, attribute parsing and
`check_mod_attrs`, AST validation, definitions and their conflicts, the resolver's errors in
signatures, impls and the entry point. `checks::check` runs them in rustc's order, on every
crate, and almost always finds nothing.

**The reference crate** is the one `dead_code` uses: 800,000 syntax nodes and 10,000
definitions. On top of those counts: about 20,000 attributes (doc comments included) on about
10,000 owners, about 40,000 paths of types and traits in signatures and about 10,000 named
lifetimes, about 2,000 impls of about 200 traits, and a few dozen staged dependencies at most
(std's crates). Cycles are at 3.5 GHz.

**The state of the art** is rustc's. `rustc_ast_passes` makes two AST walks, `AstValidator`
and the post-expansion feature-gate visitor, and `rustc_attr_parsing` lowers each attribute
once, when the AST is lowered to HIR. `check_mod_attrs` then reads the parsed attributes from
HIR without parsing them again. rustc's resolver resolves each path once, in its late pass,
with ribs pushed per item. A path it can see in a rib is found without a module lookup.
`lib_features` is a query per crate, read once per session.

### Bounds and where the checks stand

None of the "now" figures below is timed: this lane runs no build. Each one counts the
operations in the code, using the costs of the `dead_code` section.

| Component | Work | Bound | Now | Was |
| --- | --- | --- | --- | --- |
| The walk | one per check, shared with the early lints and the levels | about 3.5 ms, once | once | twice |
| The AST passes | one kind dispatch per node | about 0.7 ms | about 1.5 ms (three scans) | about 1.5 ms |
| Attribute parsing and `check_mod_attrs` | one lowering per attribute | about 2 ms | at the bound | two lowerings, two `Session`s |
| Signature paths | one resolution per path that needs one | about 10 ms | about 9 ms (estimate by count): at the bound, primitives ruled out by `crate_shadows_primitive_type` | about 30 ms |
| Signature lifetimes | one ancestor walk per lifetime | about 0.5 ms | at the bound | a `Vec` and a node clone per binder |
| Item scopes | one list per check | about 1 ms, once | once | four times |
| Impls by trait | one hash probe per impl | under 0.1 ms | at the bound | a linear search and a `Vec` per trait |
| Staged dependencies' features | one walk per dependency, only when needed | 0 for a crate with no library feature | 0 | every dependency, every check |
| Missing stability (`staged_api` only) | one visit and two probes per definition | 0 outside `staged_api`; about 1 ms for std | 0 outside `staged_api`; about 7 ms for std (estimate by count) | about 30 to 60 ms for std; before that not checked |

**The walk.** `Walk::of` visits 800,000 nodes at about 15 cycles each (cursor arithmetic on the
flat tree and a kind test), about 12 million cycles, about 3.5 ms. `checks::check` built its
own walk while the early lints and the levels used `Cx::walk`. It now uses `Cx::walk` too, so
the crate is walked once per check, not twice.

**The AST passes.** `ast_validation::check` is rustc's `AstValidator` written as one scan of
the walk's node array with a `SyntaxKind` dispatch, which matches rustc. `macros::check` and
the post-expansion gates scan the same array again. With a kind test first (no cast, no node
clone for nodes that are not macros), a scan is about 2 cycles per node, about 0.5 ms each.
One fused scan with a kind-to-check table, as the bodies' late walk has, would save two of
the three scans. It would need the passes' outputs merged in their current order, and that
is not worth doing without a build.

**Attribute parsing and `check_mod_attrs`.** rustc lowers each attribute once. Here,
`parsed` and `checked` each built a `Session` (the features, the target and the registered
tools) and each lowered every attribute: `checked` lowered them again quietly to get what
`parsed` had already made. Now `checks::check` builds one `Session` from the features
`configured` found. `parsed` lowers each owner's attributes once, into one CSR list in the
check's frame (`Lowered`: two allocations per crate), and `checked` and the stability pass
read that list back as slices. Paths are matched against rustc's tables through `with_path`,
a `SmallVec` of `&str` borrowed from the syntax, and `is`, which compares one name, instead of
building `Vec<String>`s. The builtin tools are a `const` array, so a crate that registers no
tool has an empty list. At about 300 cycles per attribute (a table lookup, an argument parse,
a parser's accept), 20,000 attributes cost about 6 million cycles, about 2 ms, which is the
bound.

**Missing stability.** rustc's `MissingStabilityAnnotations` visits each definition once. Per
definition it does one lookup of its stability and one of its effective visibility, about 50
cycles each. That is about 1 ms for std's roughly 60,000 definitions, and nothing for a crate
without `#![feature(staged_api)]`: `crate_checks` returns before it walks. Here the pass scans
the walk's `kinds` column, which is at the bound. For each definition it used to:

- walk the definition's ancestors two to four times (`order_key`, `lookup_stability`'s
  inheritance, the annotation kind, each through `parent`), at depth 3 to 6;
- map the node to its `hir` definition through `Semantics::to_def`, about 0.5 to 1 µs, to
  probe `Effective`.

That was about 30 to 60 ms for std. Now (`sol-all-diag`, 2026-10-30; not built, so an estimate
by count like the rest of this section):

- **Reachability is a probe.** The first time the check asks, it takes the crate's own
  definitions that `Effective` marks reachable and puts each one's node (`Semantics::source`)
  in a set. A node of a definition's kind is the source of exactly the definition `to_def`
  gives it, so asking the set answers what `to_def` and `Effective` answered. The one trap is a
  procedural macro, whose node is its function's, so only `macro_rules!` and `macro` nodes go in
  for macros. Building the set: about 40,000 reachable definitions at about 400 cycles (an
  interned lookup, the AST id map, a descent to the node) is 16 million cycles, about 4.6 ms.
  Each question is then one FxHash probe, about 50 cycles, about 1 ms for 60,000 definitions.
  `checks::attrs::parsers::stability::tests::reachable_by_node_is_reachable_by_to_def` checks
  the set against the old `to_def` rule, kept in the tests, at every definition of a fixture
  with each kind, private and public ones, a type reachable only through a signature, items in
  a block, the three kinds of macro and `extern` items.
- **Each definition's parent is walked once.** `Defs::parent` is memoized by the definition's
  pointer, so the two to four walks per definition become one walk and one to three probes:
  about 100 cycles saved per walk avoided, about 2.6 ms.

That puts the pass at about 7 ms for std: the set 4.6 ms, the probes 1 ms, one ancestor walk
per definition about 1.7 ms. That is still about 7 times the bound. What is left, and what
removes it:
- The set reads each definition's node by descending from the root. The AST id map already
  holds the pointer (`AstId::to_ptr`), which would take the set to about 1 ms. That needs
  `hir_def` ids for each `hir` definition kind (fields especially) in this pass. It is not done
  blind.
- The ancestor walk would go if the walk kept each node's parent index as a column (`Walk::add`
  has its stack of open nodes for this already), plus the definition kind of each node.

**Signature paths.** The work is to resolve each path that can fail. rustc resolves each path
once, and a type parameter is found in the item's rib. Here, each `PathType` paid for:
- a `String` per segment;
- the Semantics scope of the path, which builds a source analyzer;
- `resolve_path_per_ns`;
- for a path of several segments, every prefix resolved twice, once by the prefix loop and
  once by the privacy test.

Now the checks run in this order, cheapest first:
- The syntax is checked first, with nothing copied (`is_named`).
- A one-segment path naming a type parameter that the item, or the impl or trait around it,
  declares is ruled out from the syntax (`is_own_type_param`). That is rustc's rib lookup
  done without Semantics, and it only applies in a file of source, where hygiene cannot
  change what the name means.
- The scope (the module the path is in) is asked for only when an error or the privacy test
  needs it. The privacy test asks for it only when an item is not `pub`.
- A one-segment path skips the privacy test, which can never fire for it.
- The privacy test reuses the prefixes' resolutions.
- Segment names are copied only to build a diagnostic.

The item key of a signature's fact is a query, and it is now asked only when a diagnostic
fires. The signatures are visited as they are found instead of being collected. At about
1,000 cycles per resolution, for the roughly 60% of the 40,000 paths that are not ruled out
from the syntax, the cost is about 7 ms, plus about 2 ms for the syntax; the bound is about
10 ms. Primitive types (`u8`, `str`, `bool`) were the rest, about 5 ms. The syntax alone
can't rule them out, because a module or import of that name changes what hir resolves them
to. The per-crate fact (request 2, `hir_def::nameres::crate_shadows_primitive_type`, read
as `Signature::primitives`) now rules them out with one bit test outside block modules and
macro files. That puts the component at about 9 ms (estimate by count), at the bound.

**Signature lifetimes.** A lifetime is almost always declared by a binder around it. That
binder is now found by one ancestor walk with nothing collected. The binders are gathered
into a `Vec` only for the error, by a second walk. About 10,000 lifetimes at about 150 cycles
is about 0.5 ms, which is the bound.

**Item scopes.** `defs::scopes` lists every module, plus the block scopes of every body
(3,000 `Body::of` reads). It was rebuilt by `defs`, `names`, `reexports` and `impls`, and is
now made once in `checks::check` and passed down. `checks::modules` pushes each module's
children onto its stack and reverses them in place, with no `Vec` per module.

**Impls by trait.** rustc's `local_trait_impls` is a map from each trait to its impls. Here,
the grouping searched a `Vec` of traits linearly for every impl and kept a `Vec` per trait,
asked each impl for its trait twice, and searched the incoherent traits linearly. Now there is
one `FxHashMap` probe per impl. The impls of each trait are a CSR list built by counting
sort, and the incoherent traits are a `bool` per trait slot, all in the check's frame. The
per-module buffer of `impls` is reused. `denies_explicit_impls` reads the
`AttrFlags::RUSTC_DENY_EXPLICIT_IMPL` bit of each implemented trait, where it parsed the
trait's source, std's traits included (request 3, as an attribute bit).

**Staged dependencies.** rustc reads `lib_features` per crate, once. `library::check` reads a
dependency's library features (a full `Walk::of` of it) only when a feature the crate enables
is not found in the crates before it. Each dependency is read at most once per check, through
`Libraries`. Every crate's implications are read only for a stable feature, and only when
`stable_features` may fire (`Cx::lint_may_fire`), because they word nothing else. A crate that
enables no library feature reads none, which beats rustc, whose query runs once for each
dependency it loads.

**Budget tests.** In `checks/tests.rs`:
- `names_of_a_clean_crate_cost_the_same_at_any_size`: `names::check` of a crate whose
  signatures name only their own type parameters and lifetimes makes as many allocations at
  64 items as at 8, and at most 8.
- `attribute_paths_are_read_without_allocating`: `with_path` and `is` over every attribute
  make 0 allocations.
- `an_owners_lowering_is_read_without_allocating`: `Lowered::of` for every owner makes 0
  allocations.

**What blocks the last 10%.** Each item is a request in
`v2/requests/diagnostics-speed-checks.md`.
- `owners` is computed twice per check, once by `checks::check` and once by the attribute
  lints. A `Cx::owners()` cache next to `Cx::walk()` would make it once, saving about 0.6 ms
  (request 1).
- Requests 2 and 3 are done: the primitive-shadowing bit, and `#[rustc_deny_explicit_impl]`
  as an `AttrFlags` bit, which `denies_explicit_impls` reads (test
  `checks::impls::tests::explicit_impls_of_a_denying_trait_are_errors`).
- `hot_lint.py` still reports about 1,200 findings in `checks/`. The entry loops of every
  check were audited, and what they run before a diagnostic is certain is fixed above. The
  findings inside the checks' own helpers (`attrs/args.rs`, `ast_validation/*`, `defs/*`,
  `impls/*`) have not been audited line by line, and none is marked `// alloc:` yet.

## Levels, the registry and the early pass (`diagnostics-speed-lints`)

`crates/blaze-diagnostics/src/lints/`, apart from `names/` (its own sections above): `cx.rs`'s
per-check caches, `registry` (the lint table and `LintSet`), `levels` (lint levels from the
command line and attributes), `syntax` (the walk every syntax pass reads, the early lint
pass, the lexer's lints, delimiters, attributes, `check_cfg`). Reference crate as above:
800,000 syntax nodes, of which about 400,000 are of a kind that can hold an attribute, and
about 5,000 attributes, about 400 of them lint levels.

**The state of the art** is rustc:

- `ShallowLintLevelMap`: one map per HIR owner, made on first use, and a lookup that walks up
  the owner's nodes to the nearest one with a level attribute;
- `lints_that_dont_need_to_run`: the lints allowed everywhere, found once from the command
  line and every level attribute, so their passes are skipped;
- `BuiltinCombinedEarlyLintPass`: one AST walk calling every early lint at each node.

### Bounds and where the parts stand

| Component | Work | Bound | Now |
| --- | --- | --- | --- |
| Lint names | one perfect hash per name read | about 50 cycles a name | at the bound (`registry`, `LintId::named` const) |
| `Cx::lint_may_fire` | one AND of a bit | under 1 ns | at the bound |
| The set it reads (`levels::may_fire`) | one kind read per node, the names of each `ATTR` | about 0.4 ms | at the bound: one scan of the kind column |
| `Cx::lint_level` | one probe per node asked about | about 50 cycles | at the bound after the first ask; zero allocations (budget test) |
| Level maps of `decide` | one look at each node with an attribute | about 0.3 ms per map | at the bound: two columns read per node, the rest only where `Tree::attrs` is set |
| The walk (`Walk::add`) | one visit per node | about 0.5 ms | about 6 to 9 ms (estimate by count): the view's enter and leave events, and the attribute probe of each owner |
| The early pass and the syntax passes | one kind read per node, a cast where a lint wants the kind | about 0.7 ms | about 2.5 ms: one scan of the columns per pass, about ten |
| `check_cfg` | the `cfg`s of each node with an attribute | about 0.3 ms | at the bound: only nodes `Tree::attrs` marks are read |
| The lexer's lints | one lex of each source that can fire | about 2 cycles a byte where it must | a SIMD ASCII scan, and no lex, where nothing can fire |
| Suggestions (`edit_distance`) | one distance per candidate name, only for a lint that fires | about 1 cycle a byte of candidate | at the bound: Hyyrö's bit-parallel OSA distance |

The arithmetic behind each row follows.

**Lint names and `lint_may_fire`.** A name is looked up by a perfect hash over the builtin
table: one hash of a short string and one compare, about 50 cycles. The command line and
400 level attributes name under 1,000 lints, under 0.02 ms. `LintSet` is a bitset of
`LintId`s (a `u16` each). `cx.lint_may_fire(id)` is one bit test, and `cx.any_may_fire(&GIVES)`
one AND per word against a `const` set each pass declares (`LintSet::of`).

**The set it reads.** `levels::may_fire` runs once per check (a `OnceCell` in `Cx`). It reads
the kind column (2 bytes a node, 1.6 MB, at about 1 cycle a node: 0.8 million cycles, about
0.23 ms), and for each of the 5,000 `ATTR` nodes the hash of its path, about 100 cycles:
0.5 million cycles, about 0.15 ms. That is rustc's walk of every attribute, without its HIR.
Under `--cap-lints allow` (every dependency cargo builds) the set holds only what
`--force-warn` names, so every gated pass costs one AND.

**`Cx::lint_level`.** rustc builds an owner's map on first use and walks up to the nearest
node with a level. `LevelMap` keeps each set it made in a `HashTable` keyed by the node's
`SyntaxNodePtr`, hashed once with FxHash: a second ask at a node, or at any node below the
same attribute, is one probe at about 50 cycles. Budget test:
`a_level_seen_and_a_lint_that_may_fire_are_reads` (zero allocations).

**Level maps of `decide`.** `decide` builds the early and the late `Levels` over the whole
crate. Each goes over every node rustc's walk comes to, but now through two columns, kind and
`Tree::attrs`: a node may have an attribute only if it is within three nodes above an `ATTR`
(an outer attribute is a child of its owner, an inner one a child of a list at most two nodes
below it, `fn f() { #![..] }`), and `Walk::add` marks those from its stack of open nodes as
it goes. So the 400,000 nodes of owner kinds cost two byte reads each, about 0.8 million
cycles (0.23 ms), and only the few thousand marked ones are cast and have their attributes
read. Before, each of the 400,000 was cast, its parents looked at and its children scanned,
at about 100 cycles: 40 million cycles, about 11 ms, twice.

**The walk.** `Walk::add` visits the expanded crate once in pre-order and keeps four columns
indexed alike: `nodes`, `kinds` (2 bytes), `attrs` (1 byte) and the trees' `off` and
`replaced` lists. Every syntax pass of this part, `names::imports` and `names::stability`
read it through `cx.walk()`; nothing walks a tree again. The bound is a flat syntax store's
pre-order at
about 2 cycles a node, 0.5 ms. The walk is now over the flat syntax view (`syntax::view`):
no cursor is made or freed. What is left per node is two `Preorder` events (enter and leave,
each a few steps of the tree's arrays, about 15 cycles together), and for each of the
400,000 attribute owners `on()`'s scan of its children and its inner-attribute list for an
`ATTR` (about 40 cycles), plus `Item::cast` and `attrs().next()` for each item. That is
800,000 × 15 + 400,000 × 40 = 28 million cycles, about 6 to 9 ms (estimate by count). Two
things take it to the bound:
- A bit per node in the flat tree, set by the builder, for "has an `ATTR` child or an inner
  attribute list with one": an owner without it skips `on()` and the attribute-macro probe.
  The walk enters an owner before its attributes, so `Tree::attrs` can't serve here.
  Requested from frontend-tree (queue 00808).
- The walk as a scan of element numbers (`Node::descendants`), with subtree ends for
  `skip_subtree` and the stack of open nodes, in place of enter and leave events.

**The early pass and the syntax passes.** rustc's combined pass calls every early lint at
each node of its type, enabled or not. Here the early pass is one loop over the columns,
matching on the kind, and each lint runs only when its bit is set (`Builtin::unsafe_code`,
`negative_literals`), so allowed lints cost nothing per node: that beats rustc's design.
Each other syntax pass (delimiters, attributes, `check_cfg`, macros, the import scans, the two
level maps) scans the columns again, about 10 scans of 0.23 ms, about 2.5 ms. Each scan reads
1.6 to 2.4 MB, which stays in L2 between passes, and casts only the kinds it wants, each node
once. Fused into one dispatch by kind, the scans would come to the 0.7 ms bound. That fusion
is not made: each pass keeps its own state and order of what it reports, and a merged loop
would mix them for about 2 ms.

**`check_cfg`.** `unexpected_cfgs` reads the `cfg`s of every node with an attribute and of each
`cfg!`. It used to push every node of an owner kind, radix-sort them and cast each (about
17 ms). Now it reads only the nodes `Tree::attrs` marks, still in rustc's order, plus `off`,
`replaced` and the `cfg!` predicates: the 5,000 attributes at about 200 cycles is 1 million
cycles, about 0.3 ms.

**The lexer's lints.** `keyword_idents`, the 2021 prefix and 2024 guarded-string lints, the
confusable and non-ASCII ident lints need a lex of the source. Where none of them can fire,
a source that is all ASCII is not lexed: the check is the standard library's SIMD
`is_ascii`, at about 32 bytes a cycle, so the reference crate's 4 MB is 0.13 million cycles.
Budget test: `an_ascii_crate_is_not_lexed` (the same allocations at 4 and 400 functions, at
most 4).

**Suggestions.** "a lint with a similar name exists" and its kin run only on the edge, once a
diagnostic is certain, but over every candidate: the 270 lint names, or every name in scope.
`edit_distance` (by `diagnostics-lints`) is Hyyrö's 2003 bit-parallel optimal string
alignment distance, as rapidfuzz's `osa_hyrroe2003` has it. For a pattern of up to 64 bytes it
keeps one machine word per row state, so a comparison is about one cycle per byte of the
candidate instead of rustc's rows of `pattern × text` cells. It works on bytes for ASCII with
no allocation, compares upper case without making `String`s, and reuses one buffer for the
sorted-words step. Over 64 characters it takes rustc's rows. Its results are rustc's, checked
by a 20,000-pair test against rustc's algorithm. That beats rustc's own suggestion path.

### The whole part

Bound: about 3 ms for the reference crate on one core (the walk 0.5 ms, the fused scans
0.7 ms, `may_fire` 0.4 ms, the level maps 0.6 ms, `check_cfg` 0.3 ms, the lexer where it
must). Now: about 13 ms, of which 10 ms is rowan's walk and about 2 ms the unfused scans.

**Budget tests.**

- `levels`: `a_level_seen_and_a_lint_that_may_fire_are_reads`, zero allocations for
  `lint_level` and `lint_may_fire` once the map is made.
- `syntax::early::lexer`: `an_ascii_crate_is_not_lexed`.
- `syntax`: `delims_allowed_everywhere_cost_nothing` (`-A unused`, 4 and 400 functions, zero
  allocations), `the_kind_column_is_the_nodes_kinds`, `the_attrs_column_holds_every_owner`.
- `names`: `dead_code_allowed_everywhere_cost_nothing`, `item_lints_capped_cost_nothing`.

**What blocks the last 10%.**

- The walk is rowan's (front end: the flat syntax view).
- The syntax passes scan the columns once each, not once in all; fusing them is about 2 ms
  and a change of every pass's loop.
- `stability`, `imports` and `dead` resolve paths again (their sections above).
- `hot_lint.py` findings in these paths are mostly where a diagnostic is built (the edge).
  Those on the no-diagnostic path that were found have been fixed or gated on
  `lint_may_fire`. The script can't run at the moment: `tools/typed_input.py`, which it
  imports, is gone from the tree.

## MIR lowering: matches, unsafety, scopes (diagnostics-lints-mir-speed)

`crates/blaze-diagnostics/src/lower/` gives E0004 (`missing_match_arms.rs`), E0005
(`non_exhaustive_let.rs`), E0133 with `unsafe_op_in_unsafe_fn` and `deprecated_safe_2024`
(`missing_unsafe.rs`), and the scope errors: `break`/`continue` outside a loop, `return`,
`await` and `yield` outside their bodies, undeclared and unreachable labels, and
`unused_must_use`. Each lowering runs once per hir diagnostic, so a clean crate costs nothing
here beyond hir's own checks (match checking, the unsafety walk, body lowering), which are
db's. The work that counts is per diagnostic. What should stay small there is walking the
tree, not printing the message.

This builds on `diagnostics-speed-lower` ("Lowering and printing" above), which already put the
`lint_may_fire` bit test in front of `unused_must_use` and `missing_unsafe`'s two lints and
stopped `must_use_adt` collecting the type's arguments. Those stay as they are. What this
section adds is the shape of the lowerings past that gate:
- lazy walks up the tree in place of collected ancestor chains;
- one walk of a block for E0571, in place of a walk per `break`;
- label resolution that stops at the label it finds;
- E0133's block head read in place;
- `deprecated_safe_2024`'s attribute read before the gate decides;
- `Result` and `ControlFlow` found by lang item in place of a name compare;
- E0004 and E0005 printed from hir's analysis, in place of being parsed back from its text.

| Component | Work per diagnostic | Bound | Now |
| --- | --- | --- | --- |
| Scope errors (`break`, `continue`, `return`, `await`, `yield`) | one walk up to the body owner | depth `d`, about 10 cycles a step | at the bound, with no allocation |
| `break` with a value in a plain block | one walk down the block | `n` nodes of the block, once | at the bound |
| Undeclared label | a walk up to the label, then rustc's typo search | `d` steps, plus one edit distance per label in scope | at the bound |
| E0004, E0005 | print what hir found | the text of three witnesses, plus one source lookup per variant pointed at (five at most) | at the bound |
| E0133, the block's head | read the head of an `unsafe` block in place | the head's tokens | at the bound |
| `unsafe_op_in_unsafe_fn`, the first operation | which linted operation comes first in its function | one flag read | at the bound: `MissingUnsafe::first`, set in hir-ty-unsafeck's one walk (was `k·n` for `k` linted operations in a body of `n` expressions) |
| `unused_must_use` | is the type `Result` or `ControlFlow` | two lang-item field reads | at the bound, with one bit test when the lint is allowed everywhere |

**Scope errors.** rustc's `rustc_passes::loops` (`CheckLoopVisitor`) keeps a stack of contexts
on its way down: a loop, a closure, a coroutine, a labeled or unlabeled block. It resets the
stack at each body owner. rustc_ast_lowering's `lower_loop_destination` and
`rustc_resolve`'s label ribs resolve `break 'a`. blaze finds the same thing by walking up from
the error, and that walk is now lazy. `up` and `edges` step through `parent()`, a reference
count and an index in the flat tree, and expand a macro call only when they reach its root.
Each stops at the first body owner or target. On the reference crate a body is about 30 nodes
deep, so a walk is about 30 steps, about 300 cycles. Before, `ancestors` collected the whole
chain into a `Vec` up to the file root, expanding every macro above the error. `contexts`
then replayed it; that is still the case, but only up to the body owner.

**`break` with a value in a plain block.** rustc gives E0571 only for the first `break` that
targets the block. Before, blaze rebuilt the context chain of every `break` in the block for
every one of them: `B²·d` steps, plus a `Vec` per `break`. A block of 2,000 nodes with 20
`break`s cost 20·20·30 = 12,000 steps and 400 `Vec`s. Now it is one pre-order walk of the
block. Each frame records whether the block is still the target. A subtree where it is not
gets skipped, and the walk stops at the first `break` that qualifies: at most 2,000 visits,
with no allocation per node. A test compares it against the old rule on a fixture with every
kind of nesting.

**Labels.** `undeclared_label` walks up to the label used, keeping a flag for whether a closure
or a body edge sits in between (unreachable, E0767). It reads hygiene only for a label with the
same name, or one close enough for rustc's `find_best_match_for_name`. Before, it collected
every rib of the body. `yield_outside_coroutine` finds the first `yield` of a body in one
walk, which before took a walk per `yield`.

**E0004 and E0005.** rustc's `check_match` analyses a match with `rustc_pattern_analysis`:
Maranget's usefulness algorithm ("Warnings for pattern matching", 2007), extended with
constructor splitting, `PlaceValidity` and witness construction. It then prints the first
three witnesses. `hir-ty-matchck` runs that same crate. hir now gives every witness printed
(`print_witness_pat`), the variants `maybe_point_at_variant` names, the types of
`collect_special_tys`, and the flags for guards, references, private uninhabitedness and
constants. So the lowering only prints. Before, it re-inferred the scrutinee's type, split
hir's joined text, and parsed each witness back against the type (a field-type
instantiation per field). For E0005 the work left is rustc's `each_binding`: one
`resolve_bind_pat_to_const` per identifier in the pattern, and that only when rustc
suggests `let...else` or `if let`.

**E0133.** `head_range` reads the `unsafe` block's text through `SyntaxText` slices in place.
Before, it made a `String` of the whole block per error. `missing_unsafe` reads
`lint_may_fire` before it builds anything.

**Budget tests.** `a_walk_up_allocates_nothing` checks that `up` and `edges` allocate nothing
and yield one item per step. `a_blocks_breaks_are_those_of_the_plain_rule` checks that one
walk finds the `break`s the old rule found. `the_first_yield_is_that_of_the_plain_rule`,
`the_head_read_in_place_is_the_texts` and `the_witnesses_are_joined_as_rustc_joins_them`
check the rest.

**The first `unsafe_op_in_unsafe_fn` of a function.** hir-ty-unsafeck's walker keeps rustc's
`suggest_unsafe_block` as it walks (cleared at each operation it lints in an `unsafe fn`,
copied into each closure and inline const and restored after), and hir hands it on as
`MissingUnsafe::first`. The lowering reads that flag: one load. Before, it made the function's
operations again (`get_unsafe_ops`, not a query) and compared each with the operation's place,
`k·n`: about 100,000 node visits for an FFI wrapper with 50 linted operations. The old rule is
kept in the tests as the reference, and
`lower::missing_unsafe::tests::the_first_flag_is_that_of_the_plain_rule` checks the flag
against it on closures, nested closures, an inline const, an `unsafe` block and a macro's
expansion (`sol-all-diag`, 2026-10-30; not built).

hir-ty-unsafeck's flag also follows rustc's `requires_unsafe` on the lint's level (queue
00807): an operation where `unsafe_op_in_unsafe_fn` is allowed leaves the flag set for the next
one, and in an edition where the lint is allowed by default no operation is first
(test `an_allowed_operation_leaves_the_next_first` in hir-ty-unsafeck). The reference rule in
this crate's test ignores levels, so its fixture allows nothing.

**What stands in the way.**
- `rustc_pattern_analysis` is crates.io's 0.166 snapshot, not the pinned nightly's (request 2).
  This doesn't affect speed, but it lets which matches compile drift from rustc's.

## Borrow check and const check lowerings (`diagnostics-lints`)

`lower/borrowck_error.rs`, `lower/borrowck_skipped.rs`, `lower/const_check_error.rs` and
`lower/const_impure.rs` turn hir's borrow-check, const-check and purity results into rustc's
diagnostics, or blaze's. Each runs once per diagnostic hir gives and never on a clean crate,
so its work is per diagnostic. Cycles are at 3.5 GHz, with the costs of the `dead_code`
section.

**The work of one diagnostic.** These are the steps nothing can skip:

- the node from its pointer, one root-to-node descent of about 15 levels at about 20 cycles
  (300 cycles);
- its span, a binary search of the file's line index (about 200 cycles);
- the `Diag` with its message copied (one allocation, about 100 cycles);
- the item it is in, for the fact the agent API records (`Fact::Code`): one ancestor walk of
  about 10 nodes, a `to_def2` hit in Semantics' per-file cache, and the item's key (about
  800 cycles).

That is about 1,400 cycles, 0.4 µs, per diagnostic. Rendering it (`emit`, annotate-snippets)
costs 10 to 50 times more, so these lowerings are not where a failing check spends its time.

| Component | Bound per diagnostic | Now | Was |
| --- | --- | --- | --- |
| `borrowck_error` (an error) | about 1,400 cycles, plus one span per label, note and suggestion part hir gives | that, plus `phase`'s second walk (about 100) | the same |
| `borrowck_error` (`unused_mut`) | one bit test when allowed, else about 1,600 with the suggestion | at the bound | at the bound |
| `borrowck_skipped` | about 1,400 cycles, plus the item's path when printed | at the bound | twice the item lookup, about 2,200 |
| `const_check_error` | about 1,400, plus one call resolution for E0015 | about 1,500 | the same |
| `const_impure`, lint allowed | one perfect-hash lookup and one bit test, about 40 cycles | at the bound | a scan of every dependency's name, about 1,500 |
| `const_impure`, crate not opted in | 0 (hir should not look) | 0 here: hir gives no `ConstImpure`, having read the memoized `uses_fn_cache` once per `const fn` body (about 30 cycles) | about 1,500: a scan of the direct dependencies' names |

**What changed.** `borrowck_skipped` finds its item once, for both the message's path and
the fact; before, `code_fact` walked and called `to_def2` a second time. `const_impure` tests
`cx.lint_may_fire` first, so where the lint is allowed (`--cap-lints allow`, which is every
dependency cargo builds) it costs a bit test.

**Budget test.** `lower::borrowck_error::tests::an_allowed_lint_allocates_nothing`: with
`-A unused_mut -A blaze::const_impure`, lowering both allocates nothing.

**Within 10%.** `phase` (the stage rustc gives the error in) walks the ancestors a second time,
about 100 cycles, which is 7% of the bound. One walk could give both the item and the phase,
but that means changing `lints::body_item`, which is `diagnostics-speed-lints`'.

**Purity where fn-cache is not used.** hir's `check_body` now asks
`hir_ty::fn_cache::uses_fn_cache` (a salsa query per crate, so a memo read of about 30 cycles
per `const fn` body after the first) before it runs `const_purity`. A crate that does not
depend on `blaze` runs no purity analysis and gives no `ConstImpure`, which is the bound of 0
for this lowering. The lowering's own scan of the dependencies' names (about 1,500 cycles) is
gone: it reads the same query, one memo read, only for a diagnostic hir did give
(`sol-all-diag`, 2026-10-30; not built, an estimate by count).

**What stands in the way.**
- The const checker's messages are matched as strings (`starts_with("cannot call non-const
  ")`), about 20 cycles per diagnostic. That is inside the bound, but it is a string dispatch on
  a fixed set, which the `ConstCheckKind` request removes.

## The hot lint over the whole crate (`diagnostics`, session 4)

`tools/v2/hot_lint.py` counted about 5,340 findings in the crate when it was added to `HOT`.
Session 3's children brought `lints/names/**`, `emit`, `print`, the top-level files and part of
`lower/` to 0. Eight `diagnostics-hot2-*` children took the 2,316 that were left, in
`checks/**`, `lints/syntax/**`, `lints/bodies/**` and `lower/**`. The crate is now at 0. The only
exception is a lowering another agent is still writing, which it checks before it reports.

The rule was the same everywhere. A check that finds nothing is the hot path, and it is fixed.
Building a diagnostic is the edge, and it is justified on its line. Nothing a check gives
changed: no diagnostic, order, span or fact, and no test expectation. What this pass took off
the hot path:
- **Attribute parsing** (`checks/attrs/**`). An identifier, suffix, number, char, short string
  (23 bytes or less), list of up to 8 elements or meta token stream of up to 16 tokens is held
  inline (`SmolStr`, `SmallVec`). A long string literal is copied once, not twice. A lint
  attribute no longer makes a `String` per lint name.
- **Name conflicts** (`checks/defs/conflicts.rs`). Per scope: one push per binding into a
  reused buffer, one Fx hash and one probe of a reused `HashTable`, and a radix sort into reused
  scratch. Once the buffers have grown to the largest scope, nothing is allocated unless there
  is a conflict.
- **The early lints** (`lints/syntax/early/**`). `while_true` and `ambiguous_negative_literal`
  clone nothing. The confusables table is in the check's frame, with one copy per distinct
  skeleton. The lexer's per-file flags are in the frame. A node is cast only after its kind
  test.
- **Macro definitions and derive lists** (`lower/macro_*`, `lower/malformed_derive.rs`). A
  `macro_rules!` that rustc accepts costs one walk of its tokens with no allocation, and a
  derive list one walk of its children.
- **Allowed lints.** `lower/elided_lifetimes_in_path.rs` costs one bit test
  (`Cx::lint_may_fire`) where the lint is allowed everywhere.

What is left on the hot path is hir's own: the `Vec`s that `items_iter`, `use_decls` and
`extern_crate_decls` return, and the rowan handles that `qualifier()` returns. Both go when the
front end gives views and iterators (lanes frontend-tree and db). Two costs in error-free code
are inside the bound but worth a profile: `checks/impls/compare.rs` matches impl items to trait
items by linear search (O(n·m) `name` calls per impl), and `lints/bodies/types/literal.rs`
builds a `Span` for each literal that reaches `lint_literal`, which is already the gated path.

## `-Z crate-attr` refused (`diagnostics-crateattr`)

- **A crate with no refused attribute** (every crate in practice). `lower/parse/crate_attr.rs`
  costs one memoized read of `base_db::crate_attr_files` (an empty slice for a crate given
  none) and one memoized `crate_attr_error` per attribute, which is `None`. Nothing is
  allocated: the bound is those reads, and it is met.
- **Naming a span's file** (`Cx::file_name`). A file with a path costs what it did (one probe
  of the path map); only a file with none asks `base_db::crate_attr_owner`, a scan of a
  handful of attribute files, on the error path.
- **Writing** (`emit.rs` `Carried`). A diagnostic with no file without a path allocates
  nothing more: one `split_once` on `\0` per file name met. A crate attribute's file is
  indexed from the text its name carries, a few dozen bytes, once per diagnostic written.

## Lint levels as a front-end query (`diagnostics-levels`)

- **A level asked of the query again** (`hir_lint_levels::lint_level`, what the const
  evaluator reads at step 2,000,000). The bound is the salsa memo probe, a few tens of
  cycles. On top of it: one bit test of the flattened `LintSet` for a lint nothing names
  (nearly all), else a binary search of the few 2-byte ids named from the item out, a handful
  of compares in one cache line. Nothing is allocated (a test holds it at 0). It is met.
- **The first ask at an item.** Bound: reading the attributes of each node from the item out
  to the crate root, a few nodes, each one kind test, and the perfect hash of each name in a
  level attribute. The builder does that and allocates its CSR columns (a few `Vec`s sized to
  the names on the chain) and the result's two boxed slices. It is within the bound up to the
  allocator's constant.
- **An expression with attributes between it and its item** (`lint_level_at`, rare). The
  walk to the item is one kind test per node. Only when a node there has a lint attribute is a
  builder run for that expression, unmemoized: the bound is the same walk, and it is met for
  the evaluator, which asks once per evaluation that runs long.
- **The check** (`decide`, `LevelMap`, the dead-code pass). This is the code the check ran
  before, moved: the same CSR sets, one FxHash probe per node seen, one bit test per lint. A
  silent builder (`()` sink) makes no event, so the query pays nothing for what the check
  says. The one new cost is reading `-Z crate-attr`'s attributes at the crate root: one
  memoized slice read, empty for a crate given none.
