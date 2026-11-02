---
status: plan
summary: The bounds of lane pure-scripts (the purity walk, the models, the pure key's find and keep), their arithmetic, and how the code reaches them.
date: 2026-10-30
---
# pure-scripts: speed of light

For [design.md](design.md). Machine: one core of the warm VM, ~4 GHz, BLAKE3 at ~4-8 GB/s with
SIMD, a store read from mapped memory ~1 µs (page cache).

## What a pure script saves

A sandboxed run of a build script costs its compile (front end, codegen, link of the script and
of every build dependency it pulls in: 0.3-3 s cold for a typical `build.rs` with no
dependencies, 5-30 s with `syn`-sized ones) plus the sandbox's spawn and the run (~5-20 ms).
A pure hit costs the walk's MIR (front end only, memoized and kept by lane item-hash's store),
one find (section "find") and writing `OUT_DIR` back. The bound of the saving is the whole
compile and run; the pure path must cost under 10% of the sandboxed run's spawn alone on a
warm hit, i.e. under ~0.5 ms.

## find and keep

A find is the index's: one shape slot read (~1 µs), each shape variable's value from the run's
environment (a binary search in a sorted `Vec`, ~50 ns each, 2-10 names), each package file's
hash from `inputs::Files`' table (one `stat`, ~1 µs, and no read when its stamp is unchanged),
one result read (~1 µs), BLAKE3 over the entries (~100 bytes each, under 1 µs in all). For a
script that reads 3 files and 5 variables: ~6 µs, against a 5-20 ms spawn: within the bound by
three orders of magnitude. The keep happens once per miss, beside the sandboxed run's own keep:
one more shape and one more result put, ~2 µs plus the value's bytes, which the store already
hashes for the run's own key.

The check of a run against its proof is one pass over the trace's paths (a prefix compare each
against the package, `OUT_DIR` and the runtime's directories): ~20 ns a path, a few hundred
paths, under 10 µs, once per miss.

## models

`crates/hir-ty-pure/src/models.rs`. Two costs: building a toolchain crate's table, once per
toolchain, and looking a function up, once per call site the walk meets.

**The build.** std's table has ~100 entries, alloc's ~20, core's ~70. An entry is a def-map walk
of at most four segments (one `FxHashMap` probe per segment, ~20 ns each, the def map already
built for std's own analysis) and, for a method, one probe of the crate's inherent or trait impls
by simplified self type and a scan of that impl's few items: under 1 µs an entry, ~0.2 ms for all
three crates. The foreign scan visits only the modules with an `extern` block (a few dozen in
std) and compares each foreign function against 8 symbols. The bound is the def maps' and impl
tables' own reads, which the walk needs anyway; nothing here is a second pass over std's items.
It runs once per toolchain and is memoized by salsa (`crate_models`, `returns(ref)`), so its share
of a warm build is zero.

**The lookup.** `model_of` is: the function's crate (one interned-id lookup), its origin (one
salsa input read), the crate's memo (one memo fetch, no revalidation in a revision where the
toolchain did not change) and one `FxHashMap<FunctionId, Model>` probe (`FunctionId` is a `u32`,
one multiply to hash, the table under 4 KiB per crate, in L1/L2 on a walk). That is ~4
dependent loads, ~30-50 ns warm, and a user crate's function stops at the origin check, ~10 ns.
No string compare: every path is resolved to a `FunctionId` at the build. The memoized lookup
allocates nothing (`tests/models.rs`, `a_memoized_lookup_allocates_nothing`). With ~10^4 call
sites in a script's graph through std, the lookups cost ~0.5 ms cold, beside the MIR they
read; on a warm hit the walk's summaries are memoized and no lookup runs.

**Applying.** `apply_leaf` and `apply_transfer` are a `match` on the model's tag over `Abs`
values (`Copy`, 16 bytes), at most two arguments: a few ns. Only a refusal allocates, its text,
once per impure script (`tests/apply.rs`, `applying_allocates_only_a_refusal`). `allowed_env`
compares the name's text once per `EnvVar` leaf, a handful per script, not per instruction.

State of the art: the same as rustc's own lang-item and diagnostic-item tables (resolved once per
crate into an id-keyed map), with no per-query string interning on lookup.

## walk

`crates/hir-ty-pure/src/walk.rs`, `values.rs`, `identity.rs`. The walk reads the same
monomorphized runtime MIR the code generator compiles, so its floor is that MIR's, which a
build that compiles the script pays anyway; what the walk adds over it is bounded here.

**Per instance** (`summary`, a salsa query keyed by environment and instance, `returns(ref)`):
one pass over the body's statements and terminators to find its sites, with an `FxHashMap` probe
per callee's memo and a match per operand (a constant or static is evaluated only when its type
can hold a code pointer: never for the integers, `&str`s and function items that are nearly all
of them, and the evaluation is the front end's memoized `const_eval`). Then the values: a
fixpoint of mutable-borrow roots (one pass per reborrow chain link, two in practice) and one of
values. Each pass that changes something raises some local in a lattice of height 3 (`Unset`, a
value, `Unknown`), so the passes are at most `3 × locals + 1`; MIR built in block order settles in
2-3. A pass is a few ns a statement: one `Abs` (`Copy`, 16 bytes) read and joined per operand.
Then one pass over the sites applying leaves and taking in callees' summaries: each a memo
fetch, an `Effects` union (one `u8` or), a merge of at most a few sorted names, and each deferred leaf
substituted and applied again (a `match`). For a 100-statement body: ~2-5 µs beside its MIR.

**Cycles.** A strongly connected set of instances starts from the empty summary and iterates
until no member's changes. Each iteration recomputes every member; what changes only rises:
`ret` (height 3), `effects` (5 bits), `env` (the allowed names), the deferred leaves (at most
two arguments each, each of height 3) and the impurity (shortest first, so it settles once
found). Iterations are bounded by the sum of those heights, 3-4 in practice.

**Per script** (`script_purity`): a depth-first walk over the reached instances' callee lists
(one hash probe each, no body read), then BLAKE3 over 1 tag and 32 bytes of identity per instance
and 4 bytes per edge. Through std, a script reaches ~10^3-10^4 instances with ~3 edges each:
~0.5 MB hashed, ~0.1 ms at 4-8 GB/s, and ~1 ms of probes. The identities are the front end's,
memoized per body.

**Memory.** A summary is ~100 bytes plus 24 per callee (`Def`, 16, and a reference-counted
argument list): ~1-2 MB for 10^4 instances, kept by salsa, shared by every script that reaches
them. Nothing per instance is a `String` but an impure one's reason; names and deferred leaves
are inline `SmallVec`s, callees a boxed slice.

**Warm.** A revision in which neither the script nor the toolchain changed revalidates
`script_purity`'s inputs and returns the proof: toolchain instances are read at high durability,
so their summaries are not walked again. A changed script body re-runs its own instances and
any whose callees' summaries changed; the rest early-cut.

State of the art: rustc's monomorphization collector walks the same graph once per crate and
builds no summaries; this walk does as much work per instance once, then shares it across every
script and every revision through salsa, which the collector does not.
