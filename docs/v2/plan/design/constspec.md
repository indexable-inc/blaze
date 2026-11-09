---
status: plan
summary: Specializing functions on constant arguments (lane constspec, 2026-11-02) - a MIR pass that lists the calls passing a constant their callee branches on, and a copy of the callee compiled knowing it with the decided branches gone, which the code generator calls; when a call gets one, the budgets that bound the program's copies, why `#[inline(never)]` callees get copies and addressed or exported ones do not, the keys, the guarantees relied on, and the benchmark.
date: 2026-11-02
---
# Specializing functions on constant arguments

A call that passes a compile-time constant (an enum variant, a `bool`, a small integer, a
`const`) to a callee that branches on it calls a copy of the callee compiled knowing it, with
the branches it decides removed. `run(Mode::Fast, x)` calls a `run` with no `match mode`.
rustc has no such pass. LLVM's interprocedural constant propagation and function
specialization do part of it, but only within the module they are given.

The MIR side is `crates/hir-ty-mir/src/mir/optimize/specialize.rs`; its module doc is the
reference for the rules below. The code generator's side is in `crates/hir-ty-codegen` and
`crates/hir-ty-share`.

## What a copy is

- A copy is a `Specialization { base, facts }`, interned as a `CloneId`: the callee, and for
  each parameter it knows an `ArgFact`. `Is(v)` means the parameter is `v`; `IsNot([..])`
  means it is none of the values the callee compares it with. `f(5)` and `f(6)` share one copy
  of an `f` that only asks `n == 0`.
- A parameter a copy may know is a `bool`, `char`, integer or enum that the callee never
  writes, borrows or changes in place, and that it branches on: a `SwitchInt` on it, on its
  discriminant, or on `it == k` / `it != k`, through temporaries each written once.
- Lowering reads every enum's discriminant into one temporary per body, just before the switch
  on it. A switch on a temporary written more than once reads what its own block last wrote
  into it, as long as nothing borrows it. In a copy, every read of a known enum parameter's
  discriminant becomes the constant.
- Its body, `specialized_mir_body(db, clone, options)`: the base's runtime body, with the
  facts folded in and every branch they decide made a `goto`, then the base's own pipeline at
  `options`. It is generic in the base's generics, keeps the base's signature, and is passed
  the same arguments. The caller is unchanged; only its optimized body's
  `inlined.specialized` lists `(block, copy, args)`.
- The code generator calls the copy where the list says. Everything else (the interpreter,
  const eval, the walk of what a body references at `Debug`) calls the base, and gets the
  same result.

## When a call gets a copy

The decision reads only runtime bodies (`plan` and `saves` are queries over them), so it never
asks for an optimized body, and no optimized body waits on itself.

- The callee has a body, the Rust ABI, and no varargs. It is refused if any of these hold:
  - it is named by a symbol (`#[no_mangle]`, `#[export_name]`, `#[linkage]`, linked by name),
    so another object may take its calls over;
  - it is `#[track_caller]`, itself or, for a method of a trait impl, in the trait's
    declaration (the caller's location is an argument the copy does not see);
  - it is `#[cold]`, `#[optimize(none)]`, or an intrinsic;
  - it is `#[naked]`, whose assembly may not be duplicated;
  - it is `#[rustc_no_mir_inline]`: std's UB-check `precondition_check`s, called only where
    UB checks are on. rustc's MIR inliner refuses them too;
  - its calls are answered by the code generator by name (`answered_by_codegen` in
    hir-ty-mir's `mir.rs`, which matches `__blaze_cache_code`);
  - it has inline assembly. A copy would have the assembly twice, and assembly that defines a
    symbol then fails to assemble or link where rustc's one function builds. This is stricter
    than the inliner, which refuses assembly only into a caller of another `#[instruction_set]`;
  - its overflow checks are its callers' (`#[rustc_inherit_overflow_checks]`). rustc builds
    such a function with the setting of the crate that instantiates it, and one copy is shared
    by callers of every setting;
  - it is a closure or a coroutine, or its runtime body is over `MAX_CALLEE_SIZE` (2048
    statements and terminators).
- Each known argument is a constant: a literal, a variant, a free `const`, a temporary written
  once with one of those, or a parameter of the copy the call is in that the copy knows.
- The copy removes real work: at least one branch decided, and at least `MIN_SAVES` (8). A
  decided branch counts `SWITCH_COST` (2), and each statement or terminator of a block no
  longer reached counts 1. Both count `LOOP_WEIGHT` (8) times where the block is on a cycle. So
  a branch in a loop qualifies, and a one-shot `if b { 1 } else { 2 }` does not.
- Only direct calls of a function item, resolved through trait resolution, are listed. Calls
  through fn pointers, `dyn` calls, Fn-trait calls on closures, drop glue and intrinsics are
  never listed. Nothing in a coroutine body is listed.

## Budgets (rule 15)

Each budget is decided from one function alone, so no decision depends on another function or
on the order bodies are optimized in.

- **Per function**: `MAX_COPIES` (8). A parameter specialized on has its compared values'
  classes plus "not known". The parameters are taken in order, and one is skipped if the
  product of those counts, minus one, would pass `MAX_COPIES`. Two `bool`s give 3 * 3 - 1 = 8
  copies; a third `bool` is not specialized on.
- **Per body**: at most `MAX_COPIES_PER_BODY` (8) copies, together at most `GROWTH_PER_BODY`
  (4096) statements of callee.
  - When more qualify, the body takes those that remove the most work first, then the
    smaller. Copies that weigh the same are taken together or not at all.
  - Block order must not decide, not even between ties. The code generator walks a body
    optimized with UB checks on, while a release unit has them off. The checks split blocks,
    which can reorder the calls, so block order could pick different copies in the two.
- **Per program**: copies are at most `MAX_COPIES` times the code of the functions small enough
  to copy, and only copies some listed call reaches are compiled.
- **Copies of copies**: a copy's own body runs the pass too, with what it knows passed on to its
  calls. A call of the copy's own function knowing the same is the copy itself, and costs no
  budget. Any other copy it calls is one of its callee's `MAX_COPIES`, and interning ends
  repeats. So recursion and chains of copies end without a depth limit, and the bound above
  still holds.

## Profiles

No profile reaches MIR: `MirOptions` has no heat, and adding one would make every optimized
body's key depend on the profile. With a profile, the filter belongs in the code generator's one
decision point, `hir_ty_share::specialized_call`. A call is listed when it may get a copy, and
the code generator calls the copy only from a caller the profile says is hot. That makes the
profile an input of the walk, the share key and the IR queries (rule 18). `Spec` already carries
a `pgo` field, but no v2 unit's `Build` carries heat yet, so today every listed call takes its
copy. Not built.

## `#[inline(never)]`, addresses and symbols

- **`#[inline(never)]` callees get copies.** The attribute asks that the function not be built
  into its callers. A copy is still a function of its own, called with a call, with the
  callee's name in backtraces and its source in debug info, as each instance of a generic
  function already is.
  - The Reference says of `inline` in every form: "the attribute is a hint. The compiler may
    ignore it".
  - GCC's manual draws the same line ("Common Function Attributes"): `noinline` "prevents a
    function from being considered for inlining". `noclone` is the attribute that stops
    "cloning - a mechanism that produces specialized copies of functions", and `noipa` implies
    both.
  - Where a profile is wanted per function, the copy's span names the base and what it knows
    (below), so time spent in copies still adds up to the base.
- **Addresses.**
  - A copy is never the address of the callee: only direct calls are listed. A reify, a vtable
    or a fn pointer names the callee itself.
  - `std::ptr::fn_addr_eq` says function addresses have no identity to rely on: the comparison
    "may return false in any case". Even so, no comparison of addresses can see a copy.
- **Symbols.** A copy has no symbol of its own: the code generator makes it local, never under
  the base's exported name. Functions named by a symbol are refused (above).

## Identity and sharing (rule 18)

- The pass's keys are the queries' own: `plan(base)`, `saves(clone)`,
  `specialized_mir_body(clone, options)`. The `CloneId` interns the base and every fact, so two
  copies that differ in any fact are two keys.
- In the code generator:
  - a copy's code identity hashes the base's full function code (body, options, target) and then
    the facts, under its own domain;
  - its identity has a tag of its own;
  - its share key is the base's def, then the facts, over the copy's body;
  - a caller that calls a copy records the copy, never the base, so two callers that differ only
    in which copy they call get different keys.
- Identical copies from different call sites are one key, so they are compiled once and stored
  once. Copies whose code ends up equal anyway fold further through the share key and the
  dedupe after codegen (lane icf, `docs/v2/plan/design/icf.md`).
- The identity lane's `Subject` (`hir-ty-identity`) has to cover a copy (the base plus the
  facts) when it replaces the share key. Not done here.

## Pass order

`Specialize` runs last in `passes`, after `CriticalCallEdges`, where `options.inline` holds and
the body is a function's or a closure's. It changes no statement or block, so the blocks it
lists are the final ones. `renumber_blocks` keeps the list in step if a later pass is ever added,
and `validate` checks each entry is a `Call`, in block order.

- Devirtualization runs before it (before `Inline` and after it), so a call it makes direct can
  get a copy.
- Heap-to-stack (lane escape) and `BoxInPlace` may run anywhere before it. They change only
  statements in the caller, never which function a call names.
- A copy's body runs the whole pipeline, `Inline` included, so what a copy knows can also flow
  into callees inlined into it.

## The walk and the unit agree

The code generator lists what a unit references by walking bodies at `Build::WALK_INLINED`
(inlined, UB checks on). It compiles a release unit with its own options, where UB checks follow
`debug_assertions`. A copy the unit's body calls that the walk did not list ends in the code
generator's `unwalked` error: a typed error, not a fallback. Two choices keep the two lists
equal:

- The UB-check helpers are refused as callees.
- The per-body choice goes by work removed, not by block order.

What remains is the existing inliner's limit: if UB checks change what `Inline` inlines into a
body, the inlined calls differ, and so may their copies. Inlined callees already hit the same
`unwalked` error.

## Guarantees relied on

- **Same behaviour as the pinned nightly** (rule 4). A copy runs the callee's body with a
  parameter's value replaced by the value the call passes. The parameter is never written,
  borrowed or changed in place, so every read of it in the callee sees that value. The removed
  branches are the ones no execution with that value takes. Panics, overflow, drops and side
  effects of the kept code are the callee's own.
- **Addresses**: `std::ptr::fn_addr_eq`, function addresses carry no identity. Not needed for
  correctness, since copies are never addressed.
- **Inline attributes are hints**: the Rust Reference, "Code generation attributes", the
  `inline` attribute.
- **`#[naked]` bodies are not duplicated**: the Reference says the compiler may not duplicate a
  naked function's assembly, so naked functions are refused.

## Tests

In `crates/hir-ty-mir/src/mir/optimize/specialize/fixtures.rs` (run as part of `tests/mir`):

- `a_call_passing_a_mode_calls_a_copy_without_the_mode_branch`: `fast(n)` calls `run`'s copy
  knowing `Mode::Fast`. The copy reads no discriminant and has fewer statements. The copy
  knowing `Mode::Checked`, folded only, reads the mode's discriminant no more, though it still
  reads the `Option`'s into the same temporary. `either` calls two copies. An unknown mode and a
  call through a fn pointer get none.
- `values_the_callee_does_not_compare_with_share_one_copy`: `scaled(5, n)` and `scaled(6, n)` are
  one `CloneId`.
- `a_copy_computes_what_its_callee_does`: the interpreter runs each copy, folded only and as a
  release build optimizes it, and its base on the same arguments, with the same result. `run`'s
  `Checked` arm also matches an `Option`, so the shared discriminant temporary holds two enums'
  values.
- `a_copy_calls_itself_where_it_passes_on_what_it_knows`: a recursive callee's copy calls
  itself.
- `a_parameter_its_callee_writes_or_borrows_gets_no_copy`: a parameter assigned in the loop,
  or read through a reference, is never known.
- `copies_stay_within_their_budgets`: ten qualifying callees in one body get
  `MAX_COPIES_PER_BODY` copies, and the two that remove the least (one of them called first)
  are left out. Nine copies that weigh the same are all left out. Of three `bool`s, two are
  specialized on. A one-shot branch is too little work.

In `crates/hir-ty-codegen/v2/tests.rs`: `a_specialized_call_compiles_a_copy_of_its_callee`.

## The benchmark

`harness/v2/constspec.py` times `harness/v2/constspec/modes.rs` built by two blazes, `before`
(this change reversed) and `after`. The program has:

- a mode-switched hot loop (`run(mode, xs, rounds)`, a `match` per element);
- a per-element `step(mode, ..)` called from the caller's loop, where unswitching the callee's
  loop cannot help;
- `filter(odd: bool, ..)`;
- `scale(k, ..)` with `k == 0` against any other `k`.

Every callee is called with several values, so no constant reaches it by interprocedural
propagation alone.

The harness checks and records:

- that each build's 3-round checksums equal the script's own computation;
- that every timed run prints the same checksums;
- the median and minimum per loop, and the speedup;
- from the profile history (`blaze debug sql`), the `mir specialized copy` spans: `run`'s three
  copies must be there after and none before;
- the blake3 of both blazes, both programs, the source and the trees (rule 19).

For reference only, not a blaze measurement: the host's rustc `-O` on the Mac, 20000 rounds, one
run each, in ms. These come from the same rustc that checked the checksums.

| run_sum | run_mix | run_checked | step_mix | step_checked | filter_odd | filter_all | scale_0 | scale_3 |
|---|---|---|---|---|---|---|---|---|
| 5.3 | 102 | 44 | 127 | 127 | 14 | 12.6 | 6.5 | 16 |

## Not done

- **Profile-gated copies** (above).
- **Copies for calls in coroutine bodies.** The code generator's walk gives a coroutine no
  inlined pass.
- **Specializing on a constant aggregate or reference argument** (a `&'static` table, a struct
  of flags). Only scalars and enums are specialized on.
- **Measurements.** The benchmark has not been run on blaze.
