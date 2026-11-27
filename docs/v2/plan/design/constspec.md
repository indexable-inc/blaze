---
status: plan
summary: Specializing functions on constant arguments - a copy of a callee compiled knowing a constant argument it branches on, with the decided branches gone; when a call gets one, the budgets, why `#[inline(never)]` callees get copies and addressed or exported ones do not, the keys and the guarantees relied on. Not built on the item layer; the LIR clone (`specialize_body_const_param` in crates/blaze-lir/src/lto.rs) is built, not wired.
date: 2026-11-02
---
# Specializing functions on constant arguments

A call that passes a compile-time constant (an enum variant, a `bool`, a small integer, a
`const`) to a callee that branches on it calls a copy of the callee compiled knowing it, with
the branches it decides removed. `run(Mode::Fast, x)` calls a `run` with no `match mode`.
rustc has no such pass. LLVM's interprocedural constant propagation and function
specialization do part of it, but only within the module they are given.

Not built on the item layer. The clone itself is `specialize_body_const_param` in
`crates/blaze-lir/src/lto.rs`: a callee with one entry parameter fixed to an integer constant,
the signature kept, run through the whole LIR pipeline. It is built, not wired: nothing calls it.
The rules below are what the wiring decides.

## What a copy is

- A copy is a `Specialization { base, facts }`, interned as a `CloneId`: the callee, and for
  each parameter it knows an `ArgFact`. `Is(v)` means the parameter is `v`; `IsNot([..])`
  means it is none of the values the callee compares it with. `f(5)` and `f(6)` share one copy
  of an `f` that only asks `n == 0`.
- A parameter a copy may know is a `bool`, `char`, integer or enum that the callee never
  writes, borrows or changes in place, and that it branches on: a `SwitchInt` on it, on its
  discriminant, or on `it == k` / `it != k`, through temporaries each written once.
- In a copy, every read of a known enum parameter's discriminant becomes the constant.
- Its body: the base's body, with the facts folded in and every branch they decide made a jump,
  then the base's own pipeline. It keeps the base's signature and is passed the same arguments.
  The caller lists `(call, copy, args)`.
- The code generator calls the copy where the list says. Everything else (the evaluators, a
  debug build) calls the base, and gets the same result.

## When a call gets a copy

The decision reads only unoptimized bodies, so no optimized body waits on itself.

- The callee has a body, the Rust ABI, and no varargs. It is refused if any of these hold:
  - it is named by a symbol (`#[no_mangle]`, `#[export_name]`, `#[linkage]`, linked by name),
    so another object may take its calls over;
  - it is `#[track_caller]`, itself or, for a method of a trait impl, in the trait's
    declaration (the caller's location is an argument the copy does not see);
  - it is `#[cold]`, `#[optimize(none)]`, or an intrinsic;
  - it is `#[naked]`, whose assembly may not be duplicated;
  - it is `#[rustc_no_mir_inline]`: std's UB-check `precondition_check`s, called only where
    UB checks are on. rustc's MIR inliner refuses them too;
  - its calls are answered by the code generator by name (`__blaze_cache_code`);
  - it has inline assembly. A copy would have the assembly twice, and assembly that defines a
    symbol then fails to assemble or link where rustc's one function builds. This is stricter
    than the inliner, which refuses assembly only into a caller of another `#[instruction_set]`;
  - its overflow checks are its callers' (`#[rustc_inherit_overflow_checks]`). rustc builds
    such a function with the setting of the crate that instantiates it, and one copy is shared
    by callers of every setting;
  - it is a closure or a coroutine, or its body is over `MAX_CALLEE_SIZE` (2048
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
  - Block order must not decide, not even between ties: UB checks split blocks, so block order
    could pick different copies in a debug and a release body.
- **Per program**: copies are at most `MAX_COPIES` times the code of the functions small enough
  to copy, and only copies some listed call reaches are compiled.
- **Copies of copies**: a copy's own body runs the pass too, with what it knows passed on to its
  calls. A call of the copy's own function knowing the same is the copy itself, and costs no
  budget. Any other copy it calls is one of its callee's `MAX_COPIES`, and interning ends
  repeats. So recursion and chains of copies end without a depth limit, and the bound above
  still holds.

## Profiles

With a profile, a call is listed when it may get a copy, and the code generator calls the copy
only from a caller the profile says is hot; the profile is then an input of the caller's key
(rule 18). Not built.

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
    , so time spent in copies still adds up to the base.
- **Addresses.**
  - A copy is never the address of the callee: only direct calls are listed. A reify, a vtable
    or a fn pointer names the callee itself.
  - `std::ptr::fn_addr_eq` says function addresses have no identity to rely on: the comparison
    "may return false in any case". Even so, no comparison of addresses can see a copy.
- **Symbols.** A copy has no symbol of its own: the code generator makes it local, never under
  the base's exported name. Functions named by a symbol are refused (above).

## Identity and sharing (rule 18)

- A copy is the base and every fact, so two copies that differ in any fact are two keys.
- A copy's code identity hashes the base's code identity and then the facts, under its own
  domain. A caller that calls a copy records the copy, never the base, so two callers that
  differ only in which copy they call get different keys.
- Identical copies from different call sites are one key, compiled and stored once; copies
  whose code ends up equal anyway fold by [icf](icf.md).

## Pass order

Devirtualization runs before the decision, so a call it makes direct can get a copy. A copy's
body runs the whole pipeline, inlining included, so what a copy knows also flows into callees
inlined into it.

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

## Tests the wiring lands with

- A call passing a mode calls a copy without the mode branch; the copy reads no discriminant;
  an unknown mode and a call through a fn pointer get none.
- Values the callee does not compare with share one copy (`scaled(5, n)` and `scaled(6, n)`).
- A copy computes what its callee does: the evaluator runs each copy and its base on the same
  arguments, with the same result.
- A recursive callee's copy calls itself where it passes on what it knows.
- A parameter its callee writes or borrows gets no copy.
- Copies stay within their budgets: ten qualifying callees in one body get
  `MAX_COPIES_PER_BODY`, the two that remove least left out; of three `bool`s, two are
  specialized on; a one-shot branch is too little work.

## Not built

- **Profile-gated copies** (above).
- **Copies for calls in coroutine bodies.** The code generator's walk gives a coroutine no
  inlined pass.
- **Specializing on a constant aggregate or reference argument** (a `&'static` table, a struct
  of flags). Only scalars and enums are specialized on.
