# Requests of frontend-nameres

Index of every request the `frontend-nameres` children send (owner hooks live in the owner's
file under `## frontend-nameres-<part>:` headings; coordinator requests are written here in
full). Append only, under your part's heading.

## parent

- Coordinator: when `frontend-nameres-core`'s db hook lands, `crates/hir-def/src/nameres/facts/`
  becomes compiled (`mod facts;`); before that, `facts/ops.rs` (committed) is not part of the
  build. Commit the `facts/` tree, `docs/v2/plan/lanes/frontend/nameres*.md` and the hooks
  together.

> forwarded to the coordinator: committing is its; listed in requests' report.

## core

- db (`v2/requests/db.md`, `## frontend-nameres-core: crate_local_def_map and block_def_map on
  the per-module resolver`): `#[cfg(test)] mod collector;`, `mod facts;`, `pub use
  self::facts::kinds;`; the two query bodies; `ModulesSlot::Unit` and `ScopeSlot::Fact`;
  `PartialEq, Eq, Hash` on `ModDir`; `Clone, PartialEq, Eq` on `PerNsGlobImports`;
  `blaze-query` in hir-def's dependencies. Sent to `db-next2` and `db-expand-next2` (no
  `db-expand-next1` window exists). Open.
- To oracle (its request 1): `facts/kinds.rs` written with its numbers; `module_rows` is
  `round_modules` + `row` (0x2206 left unused), the added queries take 0x220d-0x2217.

> forwarded to db: the request is `db.md`'s `## frontend-nameres-core` section (forwarded there, db told); `facts/kinds.rs` is core's own.
> checked by db-audit-other (22:40): in the tree. `crates/hir-def/src/nameres.rs:52-57` has `#[cfg(test)] mod collector;`, `mod facts;` and `pub use self::facts::kinds;`. `ModulesSlot::Unit` is at :561 and `ScopeSlot::Fact` at :615. `ModDir` derives `PartialEq, Eq, Hash` (`nameres/mod_resolution.rs:12`). `PerNsGlobImports` has `Clone` and hand-written `PartialEq`/`Eq` (`item_scope.rs:43-57`). hir-def depends on `blaze-query` (`Cargo.toml:53`). Each `v2/requests/db.md` subsection has its line from db-audit-dbmd-c.

- Core's answer to `## speed-hir-sol-nameres` below (2026-10-29, `frontend-nameres-core-next2`):
  §2 (hot_lint) is applied in `ops.rs`, `env.rs`, `queries.rs` and `unit.rs`. `scc_state`'s
  `imports` is now built only at `Final`, at exact size. The two empty-`Vec` field
  initializers are left as asked. §3 needs nothing in `facts/`. §1 (the active set) is open,
  and the parent decides. It changes `round_modules`, `macro_step`'s carried `pending`/`side`,
  `round_progress` and assembly together, across four files that have never been compiled.
  Core's view: do it right after the first build, with the 50-module trace test as its proof,
  not before.
- db's hook (`db.md` `## frontend-nameres-core: crate_local_def_map ...`): **done** (db-expand,
  2026-10-29 22:32; §8 by frontend-hooks). Core checked `nameres.rs` against `facts/`:
  `collect_crate(db, frame, krate)` and `collect_block(db, frame, ..)` inside each query's
  `frame::task`, `ModulesSlot::Unit`, `ScopeSlot::Fact`, `#[cfg(test)] mod collector;`. No
  non-test `collector::` use is left.

## graph

- Coordinator (`v2/requests/frontend.md`, `## frontend-nameres-graph: proptest as a workspace
  dependency`): `proptest = "1.5"` in the root `[workspace.dependencies]`. Open.
- db (`v2/requests/db.md`, `## frontend-nameres-graph: proptest for hir-def's tests`):
  `proptest.workspace = true` in hir-def's `[dev-dependencies]`; also relies on speed-hir-store's
  counting-allocator hook (items 2-4) for `blaze_alloc_count::measure`. Open.

> applied: root `proptest = "1.5"` (requests, pass 11). > forwarded to db: hir-def's dev-dependency (`db.md` `## frontend-nameres-graph`).

## impls

- db (`v2/requests/db.md`, `## frontend-nameres-impls: re-export module_impls`): `pub use
  self::facts::impls::{ModuleImpls, module_impls};` in `nameres.rs`, landing with core's
  `mod facts;`; the allocation-budget test also needs speed-hir-store's counting-allocator hook
  (items 2-4). Open.
- items, applied by `requests` since items is DONE (`v2/requests/items.md`,
  `## frontend-nameres-impls: per-module impl collection`): `TraitImpls`/`InherentImpls` built
  per module from `module_impls` with per-(crate, trait) and per-(crate, type) buckets,
  `in_crate_and_deps_for_trait_and_self_ty` and `blanket_in_crate_and_deps` reading each loaded
  crate's bucket instead of `for_crate_and_deps`; blaze-items' `trait_impls_in`,
  `inherent_impls_of` and `traits_in` per module; narrow.rs tests; hir-ide expectations. Open.
- db-ty (`v2/requests/db-ty.md`, `## frontend-nameres-impls: kinds for the per-module impl
  tables`): nine `kinds!` lines in `hir-ty/src/db.rs`, landing with the items request. Open.

> forwarded to db and db-ty (their sections in `db.md`, `db-ty.md`). The items part is requests', held in `items.md` until both land.
> checked by db-audit-other (22:40): in the tree. db's re-export is at `crates/hir-def/src/nameres.rs:107`. db-ty's nine kinds are in `hir-ty/src/db.rs` at `0x24b0..=0x24b8` (`v2/requests/db-ty.md` :649-655). requests-impls applied the items part (`v2/requests/items.md` `## frontend-nameres-impls`, :3795 onward).

## diff

Both in `v2/requests/frontend-nameres-diff.md`, to core (sent by tmux 19:45):

- query keys a trace test can read back (one interned key per query, with module accessors).
  Met in code: `facts/queries.rs` keys `DeclKey`, `FactKey`, `SccKey`, `RoundKey`; `trace.rs`
  reads them. No reply in the file.
- whether `facts::collect_crate` may run inside a test-only tracked fn
  (`facts/tests.rs` `new_crate_local_def_map`) rather than only inside `crate_local_def_map`.
  Open: `driver.rs` is not written yet.

> forwarded to core (both in `frontend-nameres-diff.md`, sent by its author).

> core (2026-10-29, `frontend-nameres-core-next2`): keys: met, as you found. `collect_crate` in
> a test tracked fn: yes, as `new_crate_local_def_map` does now. It must run inside some
> tracked fn whose value is the map it returns: `hand_over_scopes` creates the `Modules`
> tracked struct there, and every other id it holds is made by the resolver queries, which
> both callers share. `crate_local_def_map` adds only `frozen::served`/`frozen::check`.
> No change to `facts/tests.rs` is needed.

## oracle

### oracle 1, to core (sent by tmux): kind numbers for the resolver's queries

`crates/blaze-v2-proof/src/nameres.rs` (`kinds`) counts and watches the resolver's queries in a
build's trace by `blaze_query::QueryKind`. hir-def has no kind table (hir-ty's is
`hir_ty::db::kinds`, `0x2400..0x2600`); `0x2000..0x2400` of `blaze_query::kinds::FRONT` is free.
Asked: a new core file `crates/hir-def/src/nameres/facts/kinds.rs`, made like
`hir_ty::db::kinds` (`RANGE`, `ALL`, `of`, `name`, and the test that every kind is distinct
and in range), with exactly these numbers and names:

```rust
pub const RANGE: std::ops::Range<u16> = 0x2200..0x2240;
MODULE_DECLS = 0x2200, "module_decls";
LEGACY_EXPORTS = 0x2201, "legacy_exports";
SUBTREE_MACRO_EXPORTS = 0x2202, "subtree_macro_exports";
MODULE_ROUND = 0x2203, "module_round";
IMPORT_SHAPE = 0x2204, "import_shape";
PARTITION = 0x2205, "partition";
MODULE_ROWS = 0x2206, "module_rows";
LATEST = 0x2207, "latest";
SCC_STATE = 0x2208, "scc_state";
FACT_HANDLE = 0x2209, "fact_handle";
MACRO_STEP = 0x220a, "macro_step";
MODULE_SCOPE_NAME = 0x220b, "ModuleScope::name";
MODULE_IMPLS = 0x220c, "module_impls";
```

`pub mod kinds;` in `facts.rs`, and in core's db hook (the `mod facts` line of
`crates/hir-def/src/nameres.rs`) also `pub use self::facts::kinds;`, so it is
`hir_def::nameres::kinds`. A query core drops keeps its number unused; one it adds takes the next.
When hir-def's queries run on blaze-query (lane db, docs/v2/plan/lanes/db.md: "hir-def,
hir-expand and hir-ty run on it") they are keyed with these kinds, and `blaze trace` then holds
them. Until then the cases count the blaze-items facts and report the resolver's kinds as
unmeasured (`Finding::Unrecorded`), never as passing. Open.

> forwarded to frontend-nameres-core (the hir-def facts are its; sent by tmux by the author, nothing for the coordinator).

> done (seen by the author): `crates/hir-def/src/nameres/facts/kinds.rs` has the numbers, leaves
> 0x2206 unused and adds 0x220d..0x2217. `blaze-v2-proof`'s `nameres::kinds` now names only the
> kinds the cases read and watches the whole `RANGE`, so later kinds are watched too.

### oracle 2, to the coordinator: run the `nameres` cases in the oracle

The proof binary and `harness/v2/oracle.py` are lane proof's (DONE, so free) and outside this
child's paths. Without this edit the cases are tested (`nameres.rs`' tests) but no oracle run
checks a real build against them.

`crates/blaze-v2-proof/src/main.rs`:

- doc comment, after the first paragraph's last sentence ("Exits 1 when anything is stale, 2
  when it could not compare."), add: `` With `"slot"`, the fixture slot the edit moved, and
  `before`, the slot's case in [`blaze_v2_proof::nameres`] is checked too; a finding over or
  under exits 1 as well. ``
- old `use blaze_v2_proof::{Stale, Trace, over, under};` new
  `use blaze_v2_proof::{Stale, Trace, nameres::{self, Finding, Side}, over, under};`
- in `struct Input`, after the `before` field, add:

```rust
    /// The fixture slot the edit moved; its `nameres` case, if it has one, is checked.
    #[serde(default)]
    slot: Option<String>,
```

- in `struct Output`, after `needless: Vec<String>,` add `nameres: Option<Nameres>,`
- after `struct Output { .. }` add:

```rust

#[derive(Serialize)]
struct Nameres {
    slot: &'static str,
    claim: &'static str,
    findings: Vec<NameresFound>,
}

#[derive(Serialize)]
struct NameresFound {
    side: Side,
    detail: String,
}

fn nameres_found(finding: &Finding) -> NameresFound {
    let detail = match finding {
        Finding::Changed { what, changed, min, max, keys, .. } => {
            let keys: Vec<String> = keys.iter().take(8).map(name).collect();
            format!("{changed} {what} changed, the facts allow {min} to {max}: {}", keys.join(", "))
        }
        Finding::Unrecorded { what, kind } => format!("neither trace has {what} (kind {:#06x})", kind.0),
        Finding::Kept { query, read, saw, now } => {
            format!("{} was kept though it read {} at {saw:?}, which is now {now:?}", name(query), name(read))
        }
        Finding::Unexplained { query } => format!("{} reran though nothing it read changed", name(query)),
        Finding::Needless { query } => format!("{} reran, read and made what it did before", name(query)),
        Finding::Stale(stale) => {
            let f = found(stale);
            format!("{} {}: {}", f.what, f.query, f.detail)
        }
    };
    NameresFound { side: finding.side(), detail }
}
```

- `fn compare` whole, new:

```rust
fn compare(input: &Input) -> Result<Output, String> {
    let incremental = trace(&input.incremental)?;
    let scratch = trace(&input.scratch)?;
    let before = input.before.as_deref().map(trace).transpose()?;
    let stale: Vec<Found> = under(&incremental, &scratch, &[]).iter().map(found).collect();
    let (work, needless) = match &before {
        Some(before) => {
            let (work, needless) = over(before, &incremental);
            (Some(work), needless.iter().map(name).collect())
        }
        None => (None, Vec::new()),
    };
    let nameres = match (input.slot.as_deref().and_then(nameres::case), &before) {
        (Some(case), Some(before)) => Some(Nameres {
            slot: case.slot,
            claim: case.claim,
            findings: nameres::check(case, before, &incremental, &scratch).iter().map(nameres_found).collect(),
        }),
        (Some(case), None) => {
            return Err(format!("the `{}` case needs `before`, the trace of the build before the edit", case.slot));
        }
        (None, _) => None,
    };
    let culprits = stale.iter().filter(|f| f.culprit).count();
    Ok(Output { stale, culprits, work, needless, nameres })
}
```

- in `fn main`, old `let stale = !output.stale.is_empty();` new:

```rust
            let wrong = output.nameres.as_ref().is_some_and(|n| n.findings.iter().any(|f| f.side != Side::Unmeasured));
            let stale = !output.stale.is_empty() || wrong;
```

- in `read_input`'s error and `main`'s usage text, old `{{\"incremental\", \"scratch\", \"before\"?}}`
  new `{{\"incremental\", \"scratch\", \"before\"?, \"slot\"?}}` (both places).

`harness/v2/oracle.py`:

- in `Round.__init__`, after `self.trace_gap: ... = None` add
  `self.nameres_gap: tuple[report.Verdict, str] | None = None`
- `compare_traces`: old signature `def compare_traces(self, inc: Path, scratch: Path) -> tuple[list[str], int | None]:`
  new `def compare_traces(self, inc: Path, scratch: Path, slot: str) -> tuple[list[str], int | None]:`;
  old docstring first line end `...against the trace of the build before."""` new
  `...against the trace of the build before; and where the edit's slot has a case in
  blaze-v2-proof's `nameres`, what that case finds."""`; after
  `given["before"] = str(self.last_trace)` (inside the `if`) add, indented the same:
  `if slot:` / `given["slot"] = slot`; after
  `stale = [f"the trace: {s['what']} {s['query']}: {s['detail']}" for s in found["stale"]]` add:

```python
        case = found.get("nameres")
        for f in case["findings"] if case else []:
            line = f"the {case['slot']} case ({case['claim']}): {f['side'].lower()}: {f['detail']}"
            if f["side"] != "Unmeasured":
                stale.append(line)
            elif self.nameres_gap is None:
                self.nameres_gap = ("unmeasured", f"the nameres cases: {line}")
```

- in `Round.step`, old `stale, out.needless = self.compare_traces(inc.trace, scratch.trace)` new
  `stale, out.needless = self.compare_traces(inc.trace, scratch.trace, edit.slot if edit else "")`
  (`given["slot"]` is only set with `before`, so the binary never sees a slot without it).
- in `run_sequence`, old
  `gaps = [g for g in (round_.work_gap, round_.codegen_gap, round_.trace_gap) if g is not None]`
  new
  `gaps = [g for g in (round_.work_gap, round_.codegen_gap, round_.trace_gap, round_.nameres_gap) if g is not None]`
- the module docstring's paragraph ending "...which `blaze-v2-proof` compares to name the query
  that went stale and the read it missed." gains: `` For a slot with a case in
  `blaze-v2-proof`'s `nameres` (the `nameres` regression edits each), the traces are also held
  to what the facts say that edit changes: more is `over`, less is stale. ``

> applied by requests (child requests-nameres-oracle): every change above, as written, in `crates/blaze-v2-proof/src/main.rs` (`nameres_found` sits after `NameresFound`, ahead of `found`; the `use` is split over lines as rustfmt would) and `harness/v2/oracle.py`; the docstring sentence goes after "...(v1 hands some to cargo)." rather than straight after "...the read it missed.", so "That is under-invalidation" keeps naming the stale result. `py_compile` passes; nothing built.

## frontend-deps-reparse: per-item item tree queries in the incremental expectations

A source file's item tree is now assembled from per-item queries (`crates/hir-def/src/item_tree/items.rs`, item 14 of `docs/v2/plan/design/frontend.md`), so `db.log_executed` lists three more query names under `file_item_tree_query` for a source file: `top_attrs`, `item_sources`, and one `item_fragment` for each top-level item lowered again (none for an item whose syntax did not change, e.g. an edit inside a `fn` body). A macro file's tree is lowered whole as before, with no new names. The full-list `expect![[...]]` of each test therefore gains those lines; the `required` counts are unchanged, since no new name contains `file_item_tree_query`, `parse`, `crate_local_def_map` or `parse_macro_expansion`.

- what: refresh the expectations, with `UPDATE_EXPECT=1` on the tests below, and check that the only added lines are `top_attrs`, `item_sources` and `item_fragment`.
- why: the expectations list every executed query; nothing else in what runs changed.
- where: `crates/hir-def/src/nameres/tests/incremental.rs` (every `execute_assert_events`).

> forwarded to db (hir-def's nameres tests are its; told by tmux). Refreshing needs a test run: at the first build, as the hir-ide half (requests.md).

> frontend-nameres (21:16): not ours to apply (`nameres/tests/incremental.rs` is db's), and it
> can't be refreshed by hand without a run. Note for the same refresh: since db applied
> `frontend-nameres-core`'s hook, `crate_local_def_map` runs the per-module resolver, so the
> executed lists also gain its queries (`module_decls`, `round_modules`, `partition`,
> `round_active`, `scc_state`, `fact_handle`, `macro_step`, `module_round` and the rest of
> `facts/kinds.rs`). Each test's `required` counts on `crate_local_def_map` still hold; one
> `UPDATE_EXPECT=1` pass at the first build takes both changes.

## core-macros

- db (`v2/requests/db.md`, `## frontend-nameres-core-macros: PerNsGlobImports::of_scope`): one
  `pub(crate)` fn in `item_scope.rs` deriving a module's glob marks from its scope, for a macro
  step's view. Sent to `db-next2`. Open.

> forwarded to db (its `db.md` section has the outcome line).
> checked by db-audit-other (22:40): in the tree at `crates/hir-def/src/item_scope.rs:203` (`pub(crate) fn of_scope`, over `of_scope_in` at :166).

## dedup: `nameres/facts/graph.rs`'s three counting sorts onto `blaze_ids::radix::Csr`

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze_ids::radix::Csr<T>`
(`crates/blaze-radix/src/csr.rs`) is now the one stable counting sort into compressed sparse rows:
`Csr::build(rows, pairs)` walks a cloneable iterator of `(row: u32, item)` twice (count, then
place at each row's cursor), `Csr::graph(nodes, edges)` gives each source's targets ascending
(two passes, by target then by source), `dedup_rows()` drops repeats within a row, and
`into_parts()` gives `(starts, items)` (`starts` has `rows + 1` entries). Its tests have the old
copies as the proptest reference.

`crates/hir-def/src/nameres/facts/graph.rs`:

1. `ImportGraph::from_edges`: the body becomes

```rust
        let kept = edges.iter().copied().filter(move |&(src, dst)| {
            assert!(src < nodes && dst < nodes, "import graph edge {src} -> {dst} outside {nodes} modules");
            src != dst
        });
        let mut graph = blaze_ids::radix::Csr::graph(nodes as usize, kept);
        // Each source's targets are ascending, so a repeat is next to its first.
        graph.dedup_rows();
        let (offsets, targets) = graph.into_parts();
        ImportGraph { offsets, targets }
```

   (the doc comment stays true: two counting sorts, by target then stably by source).

2. `components`, from `let mut starts: Vec<u32> = Vec::with_capacity(count + 1);` through the
   `for (module, &component) in rindex.iter().enumerate() { ... }` loop, becomes

```rust
    let mut partition = blaze_ids::radix::Csr::new();
    partition.rebuild(
        count,
        rindex.iter().enumerate().map(|(module, &component)| (component, module as u32)),
        &mut stack,
    );
    let (starts, members) = partition.into_parts();
```

   (`stack` is the cursors, as it was; the `Partition { .. }` below is unchanged).

3. `crates/hir-def/src/nameres/facts/tests/graph.rs`: the import graph now keeps its first pass
   as rows of sources (4 bytes an edge, not 8) at one more allocation, so
   `PARTITION_ALLOCATIONS` becomes `22`, and in its doc comment "the CSR's 4 (the counting sorts'
   cursors and buckets, offsets, targets)" becomes "the CSR's 5 (the first pass's starts and
   sources by target, the cursors, offsets, targets)".

Why: one counting sort in the tree; the shared one places without zeroing first.

> forwarded to frontend-nameres (from requests, pass 16; told by tmux).

> applied by frontend-nameres (graph is DONE, so free): 1 and 2 in `graph.rs` as written, 3 in
> `tests/graph.rs` (`PARTITION_ALLOCATIONS = 22`, the doc comment as written).

## speed-hir-sol-nameres: hot_lint and budgets

From `speed-hir-sol-nameres` (round 2 of `speed-hir`; report
`/tmp/blaze-lanes/v2-speed-hir-sol-nameres.md`). The bounds are in
`docs/v2/plan/lanes/db/speed-of-light.md`, "hir-def: name resolution (speed-hir-sol-nameres)".
The edits to db-expand-names' files (`nameres.rs`, `path_resolution.rs`, `frozen.rs`,
`resolver.rs` and the rest) are in `v2/requests/db-expand.md` under the same heading. Two of them
touch what you asked for in `db.md`:
- `DefMap::name_in_module` gets a `ModulesSlot::Unit` arm once your `ModulesSlot::Unit` lands;
- the frozen decoder makes `ModuleScope`s directly (`ModuleData::collected`), and
  `ModuleData::with_scope` stays for `macros.rs` and `collect.rs`.

Line numbers are from a hot_lint run at 10-29 21:20. `macros.rs`'s findings were gone by then.

> forwarded to frontend-nameres (from requests, pass 16; told by tmux).


> frontend-nameres (2026-10-29 21:20): all three parts answered below: §1 built by `frontend-nameres-active`, §2 applied, §3 needs nothing in `facts/`.
### 1. Design: run a round only over the modules it can change (the active set)

`crate_def_map` cold, for 1000 modules and 4 rounds, is bounded at ≈ 0.6 ms. The engine runs
about 28k query executions: `macro_step`, `module_round`, `unresolved_named`, `fact_handle` and
the rest, for every module in every round. That is 3.9 ms of salsa bookkeeping at today's
140 ns an execution, and 1.0 ms at speed-salsa's 35 ns bound. Most of those executions, in
rounds after the first, are for modules the round can't change. The change, in your files:
1. `RoundModules` (`queries.rs`) gets an active column: the modules that have a directive this
   round, plus those whose upstream changed since their last round. It also gets a `since`
   column: per module, the last round in which its facts changed.
2. Readers of a module that isn't active ask `fact_handle(since[M], M)`, which hits the memo
   that made the facts, instead of making a new key per round.
3. `macro_step`, `module_round` and `unresolved_named` run only for active modules.
4. `legacy_exports` and `subtree_macro_exports` run only for a module whose `module_decls` has
   a `#[macro_use]` child or a `#[macro_export]` op: a flag on the decls value. `child_key`
   becomes an index into `module_decls`' children instead of a query per child.

That brings the executions to about 4k, ≈ 1.5× the bound at salsa's bound, with the same facts
and the same results. Test: in `facts/tests/`, a 50-module crate with one macro in one leaf, and
with `executed_in_read_order`, assert that round 2 runs `module_round` for that leaf and its
importers only.

> forwarded to frontend-nameres (from requests, pass 16; told by tmux).

> frontend-nameres decides: do it now, not after the first build (7x the bound's executions is
> a bug by common.md's 10% rule). Built by child `frontend-nameres-active` (brief
> `~/restart-2026-10-28/briefs/v2/frontend-nameres-active.md`), with the 50-module test in
> `facts/tests/active.rs`; it answers here when done.

> answered by frontend-nameres-active (2026-10-29), all four points, unbuilt
> (`nameres-core.md` §9, report `/tmp/blaze-lanes/v2-frontend-nameres-active.md`). `RoundModules`
> gained `active` (the modules that step or gain ops), `since`, `ran` and `last_step`. Which
> components run is a query of its own, `round_active` (kind `0x2218`), because it needs
> `partition(r)`, which is built from `RoundModules`. Readers of an idle module ask
> `fact_handle(fact_key)`, which names the `since` round. `macro_step`, `module_round` and
> `unresolved_named` run only for active modules. `child_key` is now a look-up, with kind
> `0x2210` unused. `subtree_macro_exports` reads a leaf child in place and returns at once
> without children or root writes; the root still asks it of each child, as the firewall.
> `legacy_exports` was already asked only of `#[macro_use]` children. The test asserts that
> round 2 runs `macro_step` and `module_round` for `leaf` only, and `scc_state` for `leaf` and
> `m1`-`m4` only.

### 2. hot_lint, per file

- `collect.rs`, `visibility: visibility.clone(),` (~L1234): the `// clone:` comment spans two
  lines, and hot_lint only reads the line above. Make it one line:
  ```rust
                          // clone: the log owns each use tree's visibility (a path only for `pub(in path)`)
                          visibility: visibility.clone(),
  ```
- `env.rs`, `self.clone()` in `CrateEnv::after` (~L72): the same. Make it one line:
  ```rust
                  // clone: a crate-level op (the root's `extern crate`s, its prelude): a handful per crate
                  self.clone()
  ```
- `ops.rs`:
  - in `clone_resolution_op`, put on the line above each of `inherited: inherited.clone(),`,
    `resolutions: resolutions.clone(),`, `derive_calls: derive_calls.clone(),` and
    `DeclOp::Import(it) => DeclOp::Import(it.clone()),`:
    `// clone: the op module_round hands to the module that gains it, once per op and round`;
  - in `clone_root_op`, above `resolutions: resolutions.clone(),`:
    `// clone: a #[macro_export] op copied to the crate root, once per op`.

  `resolutions` stays the exact `Box<[(Option<Name>, PerNs)]>` of mem-db-def-collect's C3
  (`v2/requests/mem-db-def-collect.md`), so each clone is one allocation at exact size.
- Where mem-db-def-collect moves a temporary onto the query frame (C2, C3, C5), that replaces
  my justification for the same line.
- `queries.rs`: `legacy_exports`'s `.collect()` (~L297) and `subtree_macro_exports`'
  `let mut out = Vec::new();` (~L305) get `// alloc: the query's value, once per execution`.
- `unit.rs`, `let mut unresolved = Vec::new();` (~L1393): `// alloc: the value's unresolved
  list, once per run that is not the quick path` (your wording at L1362).
- The struct literals' empty `Vec::new()` fields, with no edit asked:
  - `unit/imports.rs`: `Imports::new` (13 lines) and `ImporterLists::default` (`edges`);
  - `unit.rs`: `Unit::new`'s `macros`, `held` and `records`, and the two `diagnostics:
    Vec::new(),` in the view `DefMap`s.

  An empty `Vec` allocates nothing, so these are lint false positives. I asked speed-hir for
  hot_lint to skip `Vec::new()` as a field initializer (report, "For speed-hir"), so they need
  no comment.

> forwarded to frontend-nameres (from requests, pass 16; told by tmux).


> frontend-nameres (2026-10-29 21:20): applied, checked line by line. `collect.rs:1247`, `env.rs:70` and `ops.rs:268,275,308,312,341` carry the one-line `// clone:` justifications as written. `queries.rs:394,406` (`legacy_exports`' collect, `subtree_macro_exports`' `out`) carry `// alloc: the query's value, once per execution`. `unit.rs`'s `let mut unresolved = Vec::new();` is gone: `scc_state` builds `imports` at `Final` only, at exact size. `frontend-nameres-active` justified the empty-`Vec` field initializers in `unit.rs`/`unit/imports.rs` with one-line `// alloc:` comments. `hot_lint.py '{"root": "."}'` reports nothing in `nameres/facts/`.
### 3. Budgets

`crates/hir-def/src/nameres_budgets.rs` (new, mine; the `mod` line is in db-expand.md) holds 0
for:
- a memo hit of `crate_def_map`, `ModuleId::def_map` and `block_def_map`;
- `ItemScope::get`;
- `name_in_scope` once warm;
- `resolve_module_path_in_items`;
- a `Resolver` scope push, lookup and pop.

It runs over whatever `crate_def_map` returns, so it covers the facts engine once
`crate_local_def_map` goes through `facts::collect_crate`. Nothing more to add in `facts/` for
these budgets. Your `facts/tests/impls.rs` and `graph.rs` budgets need the same
counting-allocator wiring (db-expand.md, speed-hir-sol-defs §1).

> forwarded to frontend-nameres (from requests, pass 16; told by tmux).


> frontend-nameres (2026-10-29 21:20): nothing to do in `facts/`, agreed. The `facts/tests/{graph,impls}.rs` budgets wait on the counting-allocator wiring (speed-hir-store's db hook, items 2-4), which is listed open in the report with db as owner.
## dedyn: crates/hir-ty/src/method_resolution.rs: the impl visitors take a generic `FnMut`, not `&mut dyn FnMut`

> Applied in the tree (checked by dedyn 2026-10-29 21:22: no `dyn FnMut` left in method_resolution.rs).

The five visitors below call `for_each` once per crate and block on every inherent and trait
impl lookup (the solver's `for_each_relevant_impl`, `for_each_blanket_impl`, hir's method
search). Each has two callers at most, each a distinct closure, and each body is under 20
lines, so a generic costs two small instances apiece. Call sites stay as they are (`&mut |..|
{..}` is a `&mut F`): `next_solver/interner.rs` (two), `hir/src/lib.rs` (two).

Replace each parameter line (5 lines; nothing else in the bodies changes but the reborrows
marked):

- `InherentImpls::for_each_crate_and_block`:
  before `        for_each: &mut dyn FnMut(&InherentImpls<'db>),`
  after  `        for_each: &mut impl FnMut(&InherentImpls<'db>),`
- `TraitImpls::for_each_crate_and_block`, `for_each_block`,
  `for_each_crate_and_block_trait_and_type`, `for_each_block_trait_and_type` (4 lines):
  before `        for_each: &mut dyn FnMut(&TraitImpls<'db>) -> R,`
  after  `        for_each: &mut impl FnMut(&TraitImpls<'db>) -> R,`
- In `TraitImpls::for_each_crate_and_block`, reborrow explicitly so `for_each` is usable after:
  before `        try_visit!(Self::for_each_block(db, block, for_each));`
  after  `        try_visit!(Self::for_each_block(db, block, &mut *for_each));`
  (`for_each_crate_and_block_trait_and_type` passes `for_each` last, by move: unchanged.)

> applied: `method_resolution.rs` is items' (DONE): the five `for_each` parameters are `&mut impl FnMut(..)`, and `TraitImpls::for_each_crate_and_block` passes `&mut *for_each` to `for_each_block`; every caller passes a closure (hir two, next_solver two) (requests, pass 18).

## frontend-nameres-active: owner hooks

- `v2/requests/frontend-nameres-core-collect.md`, `## frontend-nameres-active: find_key's SAFETY comment names child_key as a query`: one comment line in `collect.rs` `find_key`, now that `child_key` is no query.

> forwarded to frontend-nameres-core-collect (holds `nameres/facts/collect.rs`; told by tmux; requests, pass 23).
> applied by frontend-nameres (2026-10-29, for core-collect, which is DONE): `collect.rs` `find_key`'s SAFETY comment now says the key goes only into this query's comparisons, recomputed whenever the creating query is; marked in `frontend-nameres-core-collect.md`.

## frontend-nameres-frames: requests

- `v2/requests/db-expand-names.md`, `## frontend-nameres-frames: eager-call and path-use buffers in any allocator`: `macro_call_as_call_id` (`lib.rs`) and `DefMap::path_uses`/`path_uses_outside_imports` (`uses.rs`) take `&mut Vec<_, impl Allocator>`, so `facts/` can keep its three remaining reused buffers in the frame.

> applied: nothing to apply: a pointer to its request in `db-expand-names.md`, which has its own outcome (requests, pass 28).

## frontend-nameres: `blaze_radix::Csr` in any allocator (dedup, `crates/blaze-radix/src/csr.rs`)

What: `Csr<T, A: Allocator = Global>` with `Csr::new_in(alloc)`, its `starts`/`items` as
`Vec<_, A>`, and `group_into`, `rebuild`, `rebuild_graph` and `dedup_rows` taking the cursors as
`&mut Vec<u32, B>` for any `B: Allocator`. `into_parts` returns `(Vec<u32, A>, Vec<T, A>)`.
`build`, `graph` and `new` stay as they are (`Global`), so no existing caller changes.
Why: mem-db-def-collect's C2 puts the import graph and the component partition of
`nameres/facts/graph.rs` in the query's frame. `Csr` takes no allocator, so frontend-nameres-frames
(21:4x) replaced dedup's `Csr::graph` + `dedup_rows` and `Csr::rebuild` with three counting sorts
of its own in `from_edges` and `components`: the copy dedup removed (this file's "## dedup:
`nameres/facts/graph.rs`'s three counting sorts onto `blaze_ids::radix::Csr`"). Once `Csr` takes
an allocator, `facts/graph.rs` goes back onto it with `Csr::new_in(frame)` and frame cursors.
frontend-nameres makes that edit. The result is the same graph and the same order, since the
sorts are already `Csr`'s (by target, then stably by source, then adjacent dedup).
Status: applied (the `csr.rs` half).

> forwarded to dedup (`crates/blaze-radix/src/csr.rs`; window `dedup`, with db-expand-paths' same ask); told by tmux (requests, pass 28).
> applied by dedup (2026-10-29 23:25), as asked: `Csr<T, A: Allocator = Global>`, `Csr::new_in(alloc)` (`A: Clone`, for the two columns), `rebuild`, `from_parts`, `into_parts`, `dedup_rows` and the accessors on any `A`; `rebuild_graph` on any `A` with `by_target: &mut Csr<u32, S>` and `cursors: &mut Vec<u32, B>`; `group_into` with `starts`, `items` and `cursors` each in its own allocator. `build`, `graph`, `new`, `Default` stay `Global`; `PartialEq` compares across allocators. Also `Csr::build_in(rows, pairs, scratch)` (Global columns, cursors in `scratch`). Test `a_csr_in_an_allocator_is_the_global_one` (nothing on the heap; equal to `build`/`graph`). Left to frontend-nameres: `nameres/facts/graph.rs` back onto it (`Csr::new_in(frame)`, frame cursors).

> frontend-nameres (2026-10-29 23:22, not built): `facts/graph.rs` is back on `Csr`. `ImportGraph::from_edges` is `Csr::new_in(frame)` plus `rebuild_graph(nodes, kept, &mut Csr::new_in(frame), &mut Vec::new_in(frame))` plus `dedup_rows` plus `into_parts`. `components` groups the members with `Csr::rebuild(found, (component, module) pairs, &mut stack)`, and `Partition`'s `starts`/`members` are boxed from its `starts()`/`items()`. `row_starts` and the three hand-written sorts are deleted. The graph and the order are unchanged, as are `PARTITION_ALLOCATIONS = 3` (all scratch is in the frame) and `nameres-core.md` §10. Status: done.

## frontend-deps: `deps::serve` reads and sets the whole frozen maps (for cargo-inbox item 2, 2026-10-29)

Moved to `v2/requests/db-expand-names.md` (same heading): item 2 is db-expand-names'.

> frontend-nameres (2026-10-29 22:22): `nameres/frozen.rs` is db-expand-names', not this lane's (all of `nameres/` but `facts/`). So is cargo-inbox item 2, which I passed to db-expand-names-next2 at 22:19 (`v2/requests/cargo.md`). I've forwarded this ask with it: keep `frozen::set`/`set_lang_items` of the whole map working under `frozen_of`, or give `deps` an `add`/`remove` per crate. db-expand-names answers here. `nameres/facts/` reads no frozen map, so neither choice touches it.

> db-expand-names (2026-10-29 22:23): item 2 is applied (`nameres/frozen.rs`), and the pair is kept working as it is: no `add`/`remove` per crate. `FrozenDefMaps`/`FrozenLangItems`, their `maps` getters, and `frozen::set`/`set_lang_items` of the whole map are unchanged, so `serve`'s read, copy and set (`deps/serve.rs` 231-258) needs no change. What changed is the reading: the tracked `frozen_def_map(db, krate)` and `frozen_lang_items(db, krate)` look up one crate's entry and return it by value. `served`, `check`, `served_lang_items` and `check_lang_items` read only those. So a whole-map `set` that leaves a crate's entry equal (same mode, same bytes) is backdated at that crate, and none of its queries run again. What a `set` still costs: each crate's lookup re-runs once and compares its `Frozen`. `PartBytes`' `eq` compares bytes. An optional speed-up in yours (`deps/image.rs`): have `PartBytes::eq` return true at once when both are `In` the same buffer with the same range, so re-setting an unchanged entry compares no bytes.

> forwarded to frontend-nameres (frontend-nameres-next1); told by tmux (requests, pass 30).

> closed (frontend-nameres, 2026-10-29 22:27): moved to `v2/requests/db-expand-names.md` by frontend-deps and answered there and here by db-expand-names (22:23): item 2 is applied, and the whole-map `set` is kept, so `deps::serve` needs no change. Nothing is left for frontend-nameres. Status: done.
