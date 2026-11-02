---
status: plan
summary: blaze's `blaze::parallelizable` and `blaze::serial_dependence` — a sequential loop or iterator chain whose iterations are independent and worth running in parallel, judged on MIR; what makes a loop qualify, the effect summaries behind it, what it misses; the page each finding's help line links to.
date: 2026-10-29
---
# `blaze::parallelizable` and `blaze::serial_dependence`

`crates/blaze-lints/src/blaze/parallel.rs` and `src/blaze/parallel/`. Both are blaze's own
lints, allow by default and outside the group `blaze::hot`; turn them on with
`parallelizable = "warn"` in `[workspace.metadata.blaze.lints]`, `--warn=blaze::parallelizable` or
`#[warn(blaze::parallelizable)]`.

```rust
#[warn(blaze::parallelizable)]
fn scale(out: &mut [f32], xs: &[f32]) {
    for (o, x) in out.iter_mut().zip(xs) {
        *o = x * 2.0 + 1.0;
    }
}
```

## parallelizable

A `for` loop, or an iterator chain ending in `sum`, `product`, `count`, `min`, `max`,
`for_each`, `any`, `all` or `collect`, whose iterations are independent and whose work passes
`parallel-threshold`. The finding says why it qualifies (each note one reason) and how to run it
in parallel: rayon's `par_iter`/`par_iter_mut`/`into_par_iter`/`par_chunks*`/`par_windows`, with
the reduction named (`.map(..).sum()` for `total`), or blaze's pool
(`blaze_graph::pool::Pool::scope`) when the crate depends on it. Where the rewrite is the
source method's name alone (`xs.iter()` → `xs.par_iter()` before a chain) or a `for` loop whose
body writes only through its own item, there is a suggestion; it is machine-applicable when
the file imports `rayon::prelude` and no iteration can panic.

A loop is independent when, judged on its MIR:

- **It carries nothing but reductions.** Every local the loop starts with and writes is a
  reduction whose operator is associative and commutative: `+`, `*`, `|`, `&`, `^`, a count
  (`+= 1`), `cmp::min`/`max` or `Ord::min`/`max`, an insert into a `BTreeSet`. A float `+` or
  `*` is one only in a function marked `#[blaze::reassociate]`, and the finding says so: in
  another order it rounds differently. Integer `+` and `*` with overflow checks are exact, but
  which iteration overflows, if one does, may differ; the finding notes it.
- **It writes only memory of its own iteration.** Each write goes to a local of the iteration
  (not live where the loop starts), to the iteration's item when the iterator gives each
  iteration memory of its own (`iter_mut`, `chunks_mut`, owned items, and what `map`,
  `enumerate`, `zip`, `filter`, `take`, `skip`, `rev`, `step_by` keep of them), or to `out[i]`
  where `i` is the iteration's own index (the item of a range through adaptors that keep items
  distinct, or `enumerate`'s count) and nothing in the loop reads `out` at another index.
- **It shares nothing mutable.** What every iteration reads and none writes is shared by
  reference, so its type must be `Sync` (no `Cell`, `RefCell`, `Rc`); what each iteration is
  handed, and each reduction, moves between threads, so it must be `Send`. A `static mut`, a
  static with interior mutability, a thread-local, an atomic, a write through a pointer blaze
  can't trace to its source: not independent.
- **Its calls have no effect another order would show.** Each call's effects come from the
  callee's own summary ([below](#effect-summaries)): what memory of its arguments it writes,
  where the pointers it returns come from, whether it does I/O, touches shared state, or can't
  be seen into. A panic is allowed: in any order the loop panics if an iteration does (what the
  others wrote before it differs; the finding notes it when the loop writes).

A loop's work is the MIR statements and terminators of its blocks, each call counted as its
callee's own work, blocks of an inner loop counted 16 times, times its trip count: written down
(`0..100_000`, an array, `take(n)`, `chunks(n)` of an array), else `parallel-assumed-trips`
(1000). It must pass `parallel-threshold` (50 000); both are read from `clippy.toml`. Tiny loops
are never flagged.

## serial_dependence

A loop or chain that passes the threshold and would be independent but for exactly one thing,
named, with how to remove it:

- a value carried from iteration to iteration that is not a reduction (`h = h * 31 + x`);
- a float reduction without `#[blaze::reassociate]`;
- a `Vec` or `String` pushed onto in order (collect it with rayon's `.map(..).collect()`, which
  keeps the order);
- a `HashSet` inserted into, whose iteration order depends on the order of insertion (use a
  `BTreeSet`);
- a read of `out` at an index another iteration writes (`out[i] = out[i + 1]`);
- a call with an effect the other iterations see in the order they run: I/O, a static that
  changes, an atomic.

Two or more such things, or anything blaze can't see into, and it says nothing.

## Effect summaries

`parallel/effects.rs`: a salsa query per instance (function and generic arguments, in a
parameter environment), computed bottom-up over the instance's monomorphized MIR, with a
fixpoint for recursion. A summary says:

- which memory of its parameters it may write, in two levels: what a parameter points at, and
  what is reachable deeper (`Iterator::next(&mut self)` changes the iterator; `IterMut::next`
  hands out a pointer into the slice it holds, which it does not write);
- where the pointers it returns come from (a parameter, new memory, a static, unknown);
- its effects: a foreign call, global state, an atomic, inline assembly, an untraceable raw
  write, a call blaze can't see into, a panic; and the first that rules a loop out, named.

Calls are resolved to their instance (trait methods through their impl, closures through
`Fn*::call*` to their body, `drop_in_place` to the type's drop glue, the allocator's functions
to fresh memory); a call of `dyn`, a function pointer, a foreign function or a method for a
type parameter is unknown, and unknown means no lint. `cost` and `closure_cost` give the work
estimates the same way.

The summary lives in blaze-lints for now; the front end's `hir-ty-mir` is its long-term home
(semantics in the front end), asked of lane codegen in `v2/requests/lints-parallel.md`.

## Exactness and misses

A finding is exact on what it reads: the loop's MIR, its callees' MIR, and the types the trait
solver gives. It misses, on purpose or for now:

- an accumulator reduced twice in one iteration (`a += x; a += x`), or through more than one
  temporary: a carried value to blaze;
- a write another write in the same iteration overwrites, whose read at another index is dead;
- a chain closure that writes through its item on the way (`iter_mut().map(|x| { *x += 1; *x })`);
- loops over iterators outside std's (`HashMap::iter`, a crate's own), which rayon has no
  `par_iter` for anyway;
- `while` and `loop` loops: no item, so no own index.

Proof: `parallel/tests.rs` (each qualifying shape, each reduction, each near miss) and
`parallel/prop.rs`, a differential property over random loops of a small language: a loop
gives the same result in every order of its iterations iff the lint finds it. It is `#[ignore]`d
until compiling is allowed; its oracle is the language's own meaning, and moves to the MIR
evaluator once `hir-ty-mir` is a dev-dependency.

## Speed of light

The lint runs once per body linted: one pass over the body's MIR for definitions and calls,
one backward liveness pass, and per `for` loop a walk of its blocks. Summaries are salsa
queries, computed once per instance and reused by every caller and every body. A body with no
`for` loop and no iterator consumer costs the lookup of the std items, which is the next thing
to cache (one query per crate).
