---
status: plan
summary: Lane dbsp's maintained name resolution (crates/hir-def/src/nameres/delta): groups rerun from their round-0 logs, a read index for the cutoff, per-module scope inputs, LastWrite for the impls maintainer, and where its cost is over the speed-of-light bound.
date: 2026-10-30
---
# dbsp: maintained name resolution

Design: [../../design/dbsp.md](../../design/dbsp.md) §3–6, §9, §12. Bounds:
[speed-of-light.md](speed-of-light.md). Code: `crates/hir-def/src/nameres/delta.rs` and
`delta/`. Maintainer `100`, registered with `inventory::submit!`; `crate_local_def_map` of a
crate that is not frozen reads what it published and panics if no maintainer ran.

## Layout

| file | what |
|---|---|
| `delta.rs` | `maintain` over the crates in dependency order (frozen ones skipped), the cutoff loop, publication, `LastWrite` and `CrateDelta` |
| `delta/state.rs` | `State` per crate: groups (with each one's share of the records), rows (unordered), scope inputs, read index, crate env, group and impl positions, crate data |
| `delta/input.rs` | a write as a delta of round-0 logs: groups changed, new, gone; whether an op was removed; the name-free inserts applied without a run |
| `delta/run.rs` | one run of a region on a `facts::Unit` (`Unit::region`), `collect_block`'s loop with a macro round per group |
| `delta/reads.rs` | the reads a run makes, each with its reader (thread-local, on only inside a run) |
| `delta/diff.rs` | the keys a run changed, group positions after a tree change, and the impls, derive impls and traits for `LastWrite` |
| `delta/assemble.rs` | a group's share of the records (name records, trait-item import diagnostics), and a crate's `CrateResolution` from the shares: crate data, diagnostics, name records, derive helpers |
| `delta/publish.rs` | `ScopeInput` per module, `CrateResolution` per crate, the singleton `Resolutions` |

A *group* is a round-0 module and the modules its expansions declare (all inline below it).
It is the unit of rerun: its round-0 log is replayed, its directives expanded again (the
expansions are memoized queries) and its imports resolved again.

## One write

1. `input::read`: the groups of the written files and their parents have their log asked
   again and compared with the stored copy (equal prefix and suffix, the middle as a
   multiset). A changed child list, a changed root key, an added or removed file or a changed
   source root walks the round-0 tree from `root_decl` instead, and gives the groups whose
   position changed (`diff::update_positions`). A group whose log only gained name-free ops
   (`DefineImpl`, `DefineBuiltinDeriveImpl`, `DefineUnnamedConst`, `DefineExternBlock`) does
   not run: each op goes into its kept scope at the index a run would give it
   (`delta::apply_direct`), since round-0 ops of a list are replayed before any expansion adds
   to it.
2. The region: changed and new groups; with DRed's over-delete, the transitive readers of a
   group whose change removed an op, of a group gone, and of a dependency's changed binding.
   The root runs with every group whose expansions write its scope.
3. `run::run` over the rest of the crate as the state has it (`ModulesSlot::Maintained`): the
   region's rows are swapped out of the unordered row table and put back after, as many moves
   as the region has rows.
4. The cutoff: each key whose value differs from the state's before the write wakes its
   readers outside the region (a changed or removed one transitively), and the grown region
   runs again from scratch, until no key a reader outside read changed. A changed crate env or
   std prelude, which no read notes, runs every group.
5. Publication: each scope set where it changed (`ScopeInput`, read per name by a tracked
   `name` that backdates); each run group's share of the records made again, its reads noted
   as the group's so a later change wakes it; the crate's resolution, only where its rows,
   shares, crate data or env changed; and the crate's `CrateDelta`, whose impls are those of
   the groups run, inserted into or moved.

A crate graph change, a crate first seen and a state lost to a panic run the crate from empty:
the same computation over every group (`delta::cold` is the tests' reference).

## The four differences of §6

1. Column order is position order: the tests compare the canonical projection.
2. Glob ties: `facts/unit.rs::push_res_and_update_glob_vis` rebinds a name to the glob first
   in position order (`glob_position`: the `use` item's range mapped to the module's file,
   then its own range, then its use tree) through `ItemScope::rebind_glob_*`, so the outcome
   does not depend on which glob arrived first. Facts and the maintainer share it.
3. An import resolved through a glob before an expansion in the glob's module declared the
   name. The design says rustc rejects these with E0659; it does not. In rustc a glob binding
   read through a module path (`use foo::T`) is undetermined while `foo` has unexpanded
   macros (`rustc_resolve/src/ident.rs`, `resolve_ident_in_module_globs_unadjusted`, and
   `DeclKind::determined` in `lib.rs`), so the import waits and binds the expanded item.
   E0659's `GlobVsExpanded` fires only on lexical lookups (macro names, first segments), and
   nothing in blaze emits E0659 yet. The facts unit used to mark such an import resolved at
   once and never retry it, so a cold run kept the glob's binding while a later write that
   reran only the importer got the expanded one: a history dependence. Fixed (request 00882,
   taken by this lane): `Unit::set_unexpanded` says before each import fixed point whether a
   directive is still unexpanded (`run.rs`: any group's `pending`; `facts/unit.rs::scc_state`:
   `RoundModules::pending`; `driver.rs::collect_block`: the block's `pending`), and while one
   is, `resolve_import` returns `Indeterminate` for a full resolution whose type or value
   binding is a glob of this crate (`through_glob_of`). The import is retried when what it
   read changes, and re-recording replaces its binding (`Item::is_reresolved_by`). rustc asks
   only the glob importers on the binding's chain; asking about any directive retries more
   often and binds the same, since the binding it ends on is the final state's. Macro
   bindings stay determined, as in rustc.
   `delta/tests.rs::an_import_through_a_glob_waits_for_its_modules_expansions` checks it.
4. Unresolved attributes are reseeded at quiescence, the first in group then position order.

## Cost against the bound

Within the bound: the log diff, the over-delete through the read index, one salsa set per
changed scope, and a cutoff that sets nothing for an edit that changes no binding (a body
edit reruns nothing and hands the impls maintainer only the touched items).

Glob determinacy (§6's third) adds no pass: an import bound through a glob while a directive
is unexpanded is retried only when a slot it read is written (or, in `facts/`, once at each
round its component runs, with the other carried imports), one path resolution each, and
settles once no directive is left.

Closed, each held by `delta/tests/cost.rs`, which makes every edit in a crate of 10 modules and
of 1000 and asserts the same `Work`:
- Rows stay in place: the table is unordered, a run swaps out only its region's rows and puts
  them back (`Work::moved` counts both), and the published table is ordered only when a row
  changed. A name added that three globs read moves 10 rows; a glob cycle's deletion 6.
- Records per group: each group keeps its share of the name records and trait-item import
  diagnostics, made again only when it runs. The reads that make a share are noted as the
  group's, so a key that changes later wakes it through the cutoff like its run's reads.
- Impl positions are stored per group; a tree change hands over the impls only of groups whose
  position changed. A module added after the others moves no impl.
- A name-free insert (an impl, a builtin derive impl, an unnamed const, an extern block) runs
  no group: adding an impl costs `Work { ops: 1, slices: 1 }`, mid-file as at the end.

Still over the bound, all on publication and none counted by `Work`:
- When any share or the crate data changes, the crate's `CrateResolution` concatenates every
  group's share (`O(records)` copied). Publishing per group, as scopes are, would remove it.
- When a row changes (a tree change, a group's membership), the published module table is
  rebuilt in group order (`O(m)` row clones).
- A tree change walks every group for positions, as the round-0 tree walk that found it does.
- A named item still reruns its group from round 0: its log replay and import fixpoint, about
  the group's size.

## Tests

`delta/tests.rs` and `delta/tests/{random,cost,trace,alloc}.rs` (lane `dbsp-nameres-tests`):
maintained against `delta::cold` exactly and against `facts::collect_crate` up to §6, after
random edit scripts; the work counters of single edits; the salsa events a write sets off; and
the allocation budgets of a warm write.
