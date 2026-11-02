---
status: plan
summary: The speed of light of blaze-lints in v2, per component (driver, queries, daemon request, levels, registry, suggestions, the dispatch, each group of lints, well-known items, blaze's own lints), with the arithmetic, today's count from the code and what proof measures.
date: 2026-10-30
---
# blaze-lints: speed of light

## The machine

The target machines are in [hardware.md](../../design/hardware.md): EPYC Turin, Graviton 5 and
Apple M-series. Unless a section says otherwise, the figures are these, not yet measured
(copied from [link/speed-of-light.md](../link/speed-of-light.md)):

| what | figure |
|---|---|
| clock | 3.5 GHz |
| L1 read, streaming | 64 B a cycle |
| L2 read, streaming | 32 B a cycle (about 110 GB/s) |
| one core from DRAM, streaming | about 25 GB/s |
| a socket from DRAM | about 400 GB/s (Turin, 12 channels of DDR5 at 70% of peak) |
| cores a link may use | 16 (a dev machine), 64 (a CI machine) |
| page-cache copy (`read`, `write`, `copy_file_range`) | about 10 GB/s a core |
| NVMe write | about 7 GB/s |
| system call on an open descriptor | 0.15 µs |
| system call that resolves a path | 0.4 µs |
| `mmap` plus first touch of a 4 KiB page | 0.25 µs a page |
| `mprotect` | 0.5 µs, plus a TLB shootdown of about 2 µs on each other core running the process |
| IPC | about 2 instructions a cycle for branchy code, about 4 for straight-line integer code |
| SHA-256 with SHA-NI or ARMv8 crypto | about 2 GB/s a core |
| BLAKE3 with AVX-512 | about 6 GB/s a core |

Added for this lane: a salsa memo hit is about 15 loads with a dependent chain of 5 to 6, 6 to
8 ns ([frontend/db.md](../frontend/db.md)); a heap allocation and its free about 40 ns; a
`Symbol::intern` about 30 ns (a hash and a sharded table probe).

The reference workloads: **a large crate**, 10,000 bodies in 500 files, 2 million syntax
elements, 40 MB of text, 50 packages in the workspace, clean (no finding); **an edit** of one
body of 500 elements; **a cold crate**, the same crate never linted.

## A re-lint with nothing changed

The work is one memo hit per body and per file, plus the findings' levels and the output.
A hit is about 15 loads with a dependent chain of 5 to 6, 6 to 8 ns
([frontend/db.md](../frontend/db.md)), so 10,000 bodies come to 60 to 80 µs on one core, or
under 10 µs over 16 cores. Levels are decided only for findings (one ancestor walk each), so a
clean crate pays nothing for them. **Bound: under 100 µs.** Reached by: every body is a
query (`body_lints`), its `LintSet` is interned (a `u32` compare), and the driver walks no tree
unless something was found.

**Correction (speed-sol2-small-lints, 2026-10-30): the code did not do this.** Read from
`check.rs` (`Driver::run`) as of 2026-10-29, a warm run also did, every time:
- `names_of` for every file: a memchr scan of the whole text for `clippy::` and `blaze::`, so
  40 MB at about 10 GB/s, **4 ms**, 40 times the bound, plus a parse-tree walk per name found;
- the crate's module list, its def map's every scope (`bodies_of`), and for each of the
  10,000 bodies its block scopes and `body_syntax`/`anchor_offset` (three to four memo hits a
  body, 30 µs) and a `Vec` of the file's nesting cloned per file.

Now (this round): `query::lint_names(file)` holds the scan's result and runs again only when
the file's text changes, and `query::lint_plan(krate)` holds the module files with their `mod`
ranges and the bodies in lint order with their anchors (`BodyAt`): a warm run is one hit of
`lint_plan`, one of `lint_names` per file (500 × 7 ns = 3.5 µs), then per body a compare of
the file's name set with what is already on (it folds the names only when they could add a
lint) and one `body_lints` hit. The nesting key is built from the parent chain only for a
finding (`Driver::nesting`). What is left above the bound is the daemon's, below: the name of
each file (`name_of`, one `String` a file) and each package's setup.

## An edit of one body

The work is the edited body's tree walked once, each node dispatched on its kind, and the
typed lints' lookups for the few nodes their syntax filter passes. A body of 500 elements
walked through views is 500 steps of column arithmetic (2 to 3 ns each with the kind
dispatch, a `u16` tag `match`), about 1.5 µs; the type lookups hit `infer`, which the
check already ran for the same body (a memo hit). **Bound: a few µs, on top of the check's
own re-inference of that body.** Reached by: the query reads `body_syntax`, anchor-relative,
so no other body of the file runs again (rustc's lint passes run over the whole crate on every
build, the state of the art this beats: its late lints are not incremental); views, so the
walk counts and allocates nothing; types only asked for after a syntactic filter.

**Correction (speed-sol2-small-lints, 2026-10-30):** the per-node dispatch was not a tag
`match`: see "The dispatch" below (about 40 calls and 20 casts a node before, one `match` now).
The well-known items a typed lint compares with cost about 300 ns and an allocation per ask
before, up to four asks per `.to_string()`: see "Well-known items".

## A cold crate

The work is every element of the crate walked once: 2 million elements at about 2.5 ns is
5 ms of compute on one core, and the kind column (2 bytes an element) is 4 MB to read,
0.2 ms of bandwidth, so it is compute-bound. Bodies are independent queries and spread over
the cores (`Db::run_all`): **bound: 5 ms divided by the cores, about 0.4 ms on 16**, plus
inference, which the check has done. Which lints may be on is one scan of the text for
`clippy::`, `blaze::` and level attributes (memchr, about 10 GB/s: 4 ms for 40 MB, once per
file, not once per lint), each name found looked up by the registry's perfect hash.

## `redundant_clone`

The one lint that walks more than its node: for a `.clone()` that passes the syntactic and
type filters (a local, by autoref, not `Copy`), it walks the body's tree once for the local's
other mentions, a compare of a node kind each and a resolution (a memo hit) for each path.
A body of 2,000 elements with 4 such clones is 8,000 elements, about 20 µs at 2.5 ns, and
clones that pass the filters are a few per body. **Bound: one extra walk of the body per
candidate clone**, reached because every cheaper test runs first; a body with none pays
nothing past the filters.

## blaze's `reaches_alloc`

One query per function, memoized for the daemon's life: a function's answer is its calls'
answers ORed, a call one load of a memo. The cold cost is inferring the bodies of what the hot
code calls, std's among them: a few thousand bodies, which a build infers anyway for codegen.
**Bound: one memo hit per call site once warm.** Cycles start `false` and settle in one pass.

**Correction (speed-sol2-small-lints, 2026-10-30):** before the memo, each call site summed
`walk().count()` over every generic argument's type to refuse types longer than `LONGEST`
(256): a call with long argument types walked all of them, thousands of nodes, on every
re-lint. `alloc::fits` now stops at node 257, so a call is charged at most `LONGEST + 1`
steps before its memo (a proptest holds it equal to the sum's answer).

## What stands in the way

- Until `hir::BodyAnalysis` exists (requested of lane `db`), a typed lookup has no way to
  ask the relative tree; the lints are written against it.
- Macro calls in a body are expanded through salsa's `parse_macro_expansion`, a memo hit
  each; the expansion trees are views as well.

## The driver: `check.rs` (speed-sol2-small-lints)

**Work, counted.** Per run: the plan (one hit), each file's names (one hit), the item query
per file with a lint on (one hit), the body query per body (one hit), and per finding: the
diagnostic cloned out of the query's value, placed in its file (`place_diag`), its order key
(a `Vec` of the file's nesting, two to four pairs), its level (`sets_at`: an ancestor walk with
each owner's attributes parsed, then `Levels::of`), then one sort of the pending findings.
For the clean large crate: 500 + 10,000 hits, no finding.

**Bound.** 10,500 hits × 7 ns = 74 µs on one core. Per finding: the clone and placing (about
1 µs, the diagnostic's strings), the ancestor walk (depth about 10, 2 attribute scans each,
under 1 µs), so about 2 µs a finding; a crate with 100 findings adds 0.2 ms, all of it in the
output's own size (the diagnostics are written out anyway).

**Today.** At the bound for the clean crate since this round (above). Per finding: the order
key is one `Vec` of 2 to 4 pairs (`// alloc:` once per finding), the clone is the query's value
handed to the caller, which owns what it emits: at the bound. The sort is by `(phase, key)`
over the pending findings: n log n compares of short vectors, under 10 µs for 100.

**Gap.** None in the driver. The `File.name` strings come from the caller's `name_of` (see
the daemon request).

**proof measures.** Allocations of a warm `check` of a clean crate (budget: one per file for
its name and the driver's own vectors, none per body); memo hits per warm run (one per file
and body, plus one); time per finding.

## The queries: `query.rs` (speed-sol2-small-lints)

**Work.** `body_lints(def, set)` and `item_lints(file, krate, set)` per body and file;
`lint_names(file)`, `lint_plan(krate)`, `exported(krate)` and `famous_defs(krate)` per file or
crate. A `LintSet` is interned once per distinct set a crate uses (a crate uses 1 to 3).

**Bound.** A hit each (7 ns). A miss: the work of the walk it runs. Values hold ranges relative
to the query's tree and no names, so moving a body backdates it.

**Today.** At the bound. The `LintSet` is interned with the configuration's value (`Conf`
hashed by its table, order-free): once per set per run, about 1 µs with a `clippy.toml` of a
few keys, cached per run in `Driver::sets`.

**proof measures.** Memo hits and misses per run; the backdating of a body moved by an edit
above it (no `body_lints` re-executes).

## The daemon request: `serve.rs` (speed-sol2-small-lints)

**Work.** Per request: each package's setup (its lint flags parsed, its `clippy.toml` found
and read), the crate of each package found, `check` per crate, each finding written twice
(text and JSON). Per file: its name relative to the root.

**Bound.** A setup changes only when the flags or a `clippy.toml` change, so a warm request
makes none: 0 syscalls. Finding a crate's package is one hash probe. A file's name is the
caller's, borrowed: 0 allocations a file. Writing is the output's size (about 1 µs a finding).
So a warm, clean request of the large crate: the driver's 74 µs.

**Today (before this round).** Each request, each package: `CommandLine::parse`, then
`conf::find` (`canonicalize`, one `realpath` resolving each component, then two `stat`s,
`.clippy.toml` and `clippy.toml`, in every directory up to the root: about 20 path syscalls,
8 µs) and `Conf::load` (a `read` and a TOML parse when found): 50 packages, **400 µs, 4 times
the warm bound**. Each crate's package was found by a linear `position` over the packages
(50 × 50 path compares, about 25 µs). Each file's name: the caller's `PathBuf` cloned, then
`to_string_lossy().into_owned()`: two allocations a file, 500 files about 50 µs.

**Changed.** `Setups` keeps each package's setup by manifest directory and what it was made of
(flags, `CLIPPY_CONF_DIR`, `rust-version`); `answer_with` takes it from the caller, so a warm
request parses and reads nothing (`Setups::forget` when a `clippy.toml` changes); `answer`
keeps its signature over a fresh `Setups`. The packages are indexed by directory once per
request (first one kept, as `position` did). `path_of` may hand out a borrowed `&Path`
(`P: AsRef<Path>`), so the caller need not clone one per file. Test:
`a_setup_is_kept_until_forgotten`.

**Gap.** The daemon still calls `answer`, cloning a `PathBuf` per file, and has no `Setups`:
queue **00924** (codegen, `crates/blaze-engine/src/check.rs`, the exact edit: borrow the
path, hold `Setups`, forget it on a watched `clippy.toml` change). After it, what is left per
file is the name's one `String`, about 50 ns, 25 µs for 500 files: within the bound's margin
but not free; a name interned once per file by the daemon (`NameOf` returning `&str`) would
remove it, a change of the public `NameOf` type to make with 00924's owner.

**proof measures.** Syscalls per warm lint request (budget 0 past the socket's); allocations
per file per warm request (budget 1 today, 0 with interned names); request latency for the
large crate, clean.

## Level resolution: `level.rs`, `conf.rs` (speed-sol2-small-lints)

**Work.** Once per run: `Levels::new` (the command line's sets) and `command_line_on` (each
of the 43 clippy lints and 6 blaze lints asked their level with no attribute: 49 probes of
the command line's few sets). Per finding: `Levels::of` (a probe of the command line's sets
and the attributes' sets, then `warnings`, then the cap). `Conf::bool`/`int` per lint run
with a key: a lookup in the TOML table (a `BTreeMap` of a few keys or an `IndexMap`), 10 to
30 ns. fix-names owns the `LintId` level bitsets; nothing here touches them.

**Bound.** Per run: 49 × (a few compares) under 1 µs. Per finding: the sets in scope, a few
tens of compares, under 0.5 µs. A configuration read: one probe.

**Today.** At the bound: levels run per finding, not per node; `Target::covers` compares a
lint's index or group. `Conf` lookups happen per candidate a lint already passed its syntactic
filter for (`avoid-breaking-exported-api`, `type-complexity-threshold`), a few per body:
under 0.1 µs a body, within 10% of the body's walk.

**proof measures.** Time of `Levels::of` per finding; `command_line_on` per run.

## The registry and name lookup: `registry/` (speed-sol2-small-lints)

**Work.** A name to clippy's lint or group: `registry::named`, from the text scan (per name
written) and the command line (per flag).

**Bound.** One perfect hash of the name: a hash of its bytes (a multiply per 8 bytes), two
dependent loads, one compare: about 5 ns.

**Today.** At the bound: `blaze_phf::phf_table!` (PTHash's scheme, built when the crate
compiles) over the 800 lint names and the renamed and removed ones; `Implemented::of` is one
load from a table indexed by clippy's lint index (`BY_REGISTRY`).

**proof measures.** None beyond the text scan's time per file.

## Suggestions: `sugg.rs` (speed-sol2-small-lints)

**Work.** Per finding with a suggestion: the replacement text built from the written source
(clippy's `Sugg`: parenthesize by precedence, `!`, `&`, `*`), one `String` of the output's
length.

**Bound.** The output's bytes: the replacement is what is written, a few tens of bytes.

**Today.** At the bound: built only once a finding is certain; the hot lint's string findings
here are on that path (per finding, never per node).

**proof measures.** Nothing on a clean crate (budget 0 calls); allocations per suggestion.

## The dispatch: `lints/mod.rs` (speed-sol2-small-lints)

**Work.** Per node of a walked tree: the lints of the node's kind, each running its filter.

**Bound.** One `match` on the node's `u16` kind, one indirect jump, then only the lints of
that kind: 2 to 3 ns a node with no candidate (the walk's own step), 2 million elements 5 ms
cold (the cold section's figure).

**Today (before this round).** `check_node` asked every lint: `Item::cast`, then a chain of
up to 10 casts (`Enum`, `Impl`, `Fn`, …), then `Expr::cast` and 13 expression lints, each
matching its variant or a method name itself (`utils::method_name` compares strings), then 17
node lints each casting again: about 40 calls and 20 kind compares a node, 30 to 40 ns, **10
times the bound**; a method call was compared with every method name by every lint (about 25
string compares). The walk scanned a node's children for attributes up to four times
(`enabled`, `expectations_of`, `walk_attr_expansion`, `walk_derives`).

**Changed.** One `match node.kind()` to the lints of that kind, in the order the casts ran
them; a method call does one perfect hash of its name (`METHODS`, 15 names) to the lints that
look at that name. The attributes are scanned once a node (`has_attrs`): a node without
attributes is neither `cfg`'d out nor expanded by an attribute or a derive. Tests: every
fixture is linted twice, once by the old cast chain (`check_node_by_casts`, kept under
`cfg(test)`) and once by the dispatch, on fresh databases, and the outputs compared
(`tests::lint_with`); the method table finds each name at its index and no other; every item
and ADT kind is an `AnyHasAttrs`.

**Now.** Within 10%: the per-node cost is the walk's step plus one `match`.

**proof measures.** ns per element of a cold walk of a large crate with every lint on.

## Syntax-only lints (speed-sol2-small-lints)

`bool_assert_comparison`, `crate_in_macro_def`, `dbg_macro`, `empty_enums`, `needless_return`,
`nonstandard_macro_braces`, `print_stdout`, `print_stderr`, `redundant_static_lifetimes`,
`todo`, `upper_case_acronyms`, `collapsible_if`, `needless_bool`, `byte_char_slices`,
`undocumented_unsafe_blocks`, `match_like_matches_macro`, `large_enum_variant` (syntax first),
`useless_vec`/`useless_format`/`useless_borrows_in_formatting` (a macro call's name first).

**Work.** Per node of their kind: a few child reads; per candidate: the source text it
quotes. **Bound.** A few ns a node of their kind; output-sized work per finding. **Today.**
At the bound: each runs only on its kind (above), reads views (no allocation), and builds text
only for a finding (`undocumented_unsafe_blocks` scans the lines above an `unsafe` block for a
`SAFETY:` comment: the bytes of those lines, at the bound). **proof measures.** ns per node
of each kind; allocations of a clean walk (budget 0).

## Typed lints after a syntactic filter (speed-sol2-small-lints)

`clone_on_copy`, `clone_on_ref_ptr`, `get_first`, `implicit_clone`, `inefficient_to_string`,
`io_other_error`, `map_clone`, `map_unwrap_or`, `needless_borrow`,
`needless_borrows_for_generic_args`, `single_char_add_str`, `str_to_string`,
`unnecessary_map_or`, `unnecessary_to_owned`, `format_collect`, `box_collection`/`vec_box`/
`rc_buffer`, `large_types_passed_by_value`, `needless_pass_by_value`, `new_without_default`.

**Work.** Per candidate (a method call of their name, a `&` expression, a signature type):
the method's resolution, a type or two (memo hits into the body's inference), and the
well-known items they compare with. **Bound.** A few memo hits, about 50 ns a candidate, and
a few candidates a body. **Today.** At the bound but for the well-known items (next section),
which were 300 ns an ask and up to four asks a candidate. `needless_pass_by_value` and
`large_types_passed_by_value` ask the layout of each parameter type: a memo hit each.
**proof measures.** Memo hits per candidate; allocations per candidate (budget 0 until a
finding).

## Well-known items: `famous.rs` (speed-sol2-small-lints)

**Work.** A lint asks for a std item by path (`["alloc", "string", "String"]`): up to four
asks a `.to_string()`/`.to_owned()` candidate (`ToString`, `String`, `Deref`, `AsRef`), three
for each `HashMap` path type (`std_hashmap`), one or two per `Option`/`Result` check.

**Bound.** The items depend only on the crate's dependencies, so resolving them is once per
crate; an ask is reading the key it holds (a hash of 20 to 26 bytes, about 5 ns) and one load:
about 10 ns.

**Today (before this round).** Each ask joined the path into a heap `String`, scanned the
crate's dependencies for the lang crate, interned each segment (`Symbol::intern`, 30 ns
each), and probed each module's scope: about 300 ns and one allocation, **30 times the
bound**, up to 1.2 µs a candidate, as much as the walk of the body around it.

**Changed.** `famous::KNOWN` lists every path a lint asks (84: the literals of the lints, and
the `std::` copies `parallel/iter.rs` asks for when `core` or `alloc` is not a dependency);
`query::famous_defs(krate)` resolves them all once per crate; an ask joins the segments in a
buffer on the stack and does one perfect hash (`KNOWN_AT`) and one load, the table read once
per query (`Cx::famous_defs`). `Famous::core_result_Result` reads the same table. A path not
in the table is a bug of the crate and panics naming it (never a quiet slow path). Tests:
every path is found by its segments, the join modelled on how `Famous::find` reads a path; on
a fixture crate the table equals `Famous::find` path by path.

**Now.** Within 10%: about 10 ns an ask.

**proof measures.** ns per ask; `famous_defs` executions per crate (budget 1 per revision of
the crate graph).

## Whole-body walks (speed-sol2-small-lints)

`redundant_clone` (above), `multiple_unsafe_ops_per_block` (the block's subtree once per
`unsafe` block), `needless_borrow`'s `ref` pattern case (the body's bindings of a local).

**Work.** One walk of the body or block per candidate. **Bound.** That walk, 2.5 ns an
element. **Today.** At the bound: each walk only after the candidate passed every cheaper
test; no allocation per element (`multiple_unsafe_ops_per_block` keeps its operations in one
`Vec` per block, output-sized). **proof measures.** Elements walked per candidate.

## blaze's own lints: `blaze.rs`, `clone.rs`, `hashmap.rs`, `dispatch.rs` (speed-sol2-small-lints)

**Work.** All allowed until turned on (`blaze::hot` and its lints), so a default run pays one
bit test a node (`on_any(ANY)`). Turned on: per path type named `HashMap`/`HashSet` (the last
segment's text compared first), its resolution and the three std items; per body, from its
root once, a walk of the body's HIR expressions with inference's results: per call, the callee
(`written_callee`, a memo hit), `reaches_alloc` (a memo hit), the `Clone` impl's shape
(`clone.rs`), a `dyn` receiver test (`dispatch.rs`).

**Bound.** Off: 1 bit test a node. On: a few memo hits per call, 50 ns; 10,000 bodies of 50
calls each, 25 ms cold over one core, 1.6 ms on 16; warm, the body query's hit.

**Today.** At the bound with the well-known items fixed (above: `hashmap::Std::new` was three
300 ns asks per candidate) and the type walk bounded (`fits`, above). The exemption comment
(`exempt.rs`) is read only for a finding.

**proof measures.** Memo hits per call of a hot body; ns per body with `blaze::hot` on.

## `parallelizable` and `serial_dependence`: `blaze/parallel/*` (speed-sol2-small-lints)

Allowed by default; only a crate that turns them on pays.

- **`region.rs`**: per body, the CFG's blocks in a cycle, the natural loop of a back edge, and
  liveness. Bound: linear in blocks plus edges (an iterative dataflow over bitsets, a few
  passes): a body of 100 blocks, under 5 µs. Today: bitsets of locals per block, iterated to a
  fixpoint over reverse postorder: at the bound.
- **`deps.rs`**: per loop, each write traced to its root through the definitions of the pointer
  it goes through, each mutated local's statements in the loop scanned for a reduction. Bound:
  the loop's statements times the mutated locals, both small (under 20). Today: at the bound;
  the reduction's name is a `String` made for the message only when a reduction is found.
- **`effects.rs`**: a summary per instance, bottom-up, as salsa queries (`effects`,
  `closure_effects`, `cost`, `closure_cost`): bound one hit per call site once warm, the cold
  cost the monomorphized MIR of what the loop calls, which codegen builds anyway. Today: at the
  bound; cycles start empty and grow to their fixpoint.
- **`iter.rs`**: a chain's type taken apart from the outside in against std's iterators
  (`Std::new`, about 35 items): bound a few ns a layer once the items are known. Today: the
  items were 35 asks of 300 ns each per `Std::new` (10 µs) before the well-known items table;
  now 35 asks of 10 ns, within 10%.
- **`prop.rs`**: the property test's generator and interpreter, test code only.

**proof measures.** Time per loop judged; `effects` executions per instance (1 per revision).

## Where each gap stands (speed-sol2-small-lints)

| component | bound | before | now | queue |
|---|---|---|---|---|
| warm re-lint, the driver | 74 µs | 4 ms (text scan) + 30 µs (plan) | at the bound | |
| daemon request, setups | 0 syscalls | 400 µs (50 packages) | at the bound once the daemon holds `Setups` | 00924 |
| daemon request, file names | 0 allocations a file | 2 a file | 2 a file; 1 after 00924 | 00924 |
| the dispatch | 2 to 3 ns a node | 30 to 40 ns | within 10% | |
| attributes per node | 1 scan | up to 4 | 1 | |
| well-known items | 10 ns an ask | 300 ns + 1 allocation | within 10% | |
| `reaches_alloc` length check | ≤ 257 steps | every argument type's every node | ≤ 257 | |
| levels, registry, suggestions, queries | as above | at the bound | at the bound | |
| blaze's own, parallel | as above | at the bound but for the items | at the bound | |
