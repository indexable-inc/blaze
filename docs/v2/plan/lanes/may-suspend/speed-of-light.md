---
status: plan
summary: lane may-suspend's speed of light: the bound of the held-across check from its work (one scan of each reached instance's runtime MIR, one dataflow per body that calls something that may suspend), what `hir-ty-suspend` costs as written, and what closes each gap.
date: 2026-10-31
---
# Lane may-suspend: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Nothing here was measured: the bounds are
arithmetic from the work, and the costs as written come from reading `crates/hir-ty-suspend/`.

## A crate graph without corosensei

Work: none. The check exists for programs that park fibers.

- Bound: one look at the crate graph per crate, memoized: O(crates in its graph) once, then one
  memo probe per crate's check.
- As written: `root::roots` is a DFS over the crate's dependencies comparing each crate's name
  with `corosensei`, memoized per crate; `suspend_check` returns at once when it is empty, and
  asks for no MIR and no summary. At the bound. Test `no_corosensei_no_walk` holds it there.

## A crate graph with corosensei

Work: find which reached instances may suspend, then check the bodies that call one.

- Bound: each reached instance's monomorphized runtime MIR is the code generator's input
  already, so it is a memo hit, not work of this check. Past that, one scan of each body's
  terminators and rvalues for its call targets, vtables and reified pointers: O(statements),
  about 10 ns a statement. Then, only in a body with a may-suspend call, rustc's three dataflow
  passes (`MaybeBorrowedLocals`, `MaybeRequiresStorage`, `MaybeLiveLocals`): O(blocks × locals /
  64) words a pass, with one `Send` question per held local of a non-trivial type. Errors cost
  the path walk, O(path length) memo probes. A crate of 10,000 instances averaging 50
  statements: about 5 ms of scan, the dataflow on the few percent of bodies that wait.
- As written:
  - `summary` scans a body's sites, and `instance_check` scans the same body's sites again:
    two scans where one would do. Closing it: keep the sites a check needs (the blocks of calls
    whose targets may suspend, the slots and reified pointers that do) in a query of their own
    that both read, or have `summary` return the suspend points so the check never rescans a
    body that has none. The second is the better one: most bodies have none, and then the check
    is one probe.
  - The fixpoint over recursion: `summary`'s cycle starts at "never suspends" and keeps the
    hop with the fewest calls. A cycle reruns until no member's hop changes, at most its
    length times; recursion in real code is short, so this is a small constant.
  - `reached` walks `summary`'s callees once, O(instances + edges), one hash probe each.
  - `suspends` on each target is a memo probe per call target, O(call sites).
  - The `Send` question goes to the trait solver once per (held local, call); the solver
    caches by goal, so repeats of one type are a probe. A type with no lifetimes, no
    parameters and only scalar fields could be answered without the solver; that is a
    gap only if `proof` shows the solver in the profile.
  - The held-across dataflow (`held_across_calls_in`) runs once per body for every suspend
    point at once, in a `blaze_mem` frame: no allocation outlives the body's check.
  - Paths are built only for an error, from each summary's next hop (no path is stored).

## What `proof` should measure

- The check of `blaze-graph` and of a crate using it heavily: time in `summary` and
  `instance_check` beside the code generator's own time for the same crate. The check should
  be a few percent of codegen's, since it reads what codegen builds.
- The count of bodies the dataflow runs on, against the count of reached instances.
