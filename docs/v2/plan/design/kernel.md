---
status: plan
summary: blaze v2's kernel: registered pure compute steps from MIR down, each value kept in the store under its identity, its step's code and flags, with the narrow reads it made and their hashes, verified on lookup; codegen's dev templates are its first entries.
date: 2026-11-02
---
# The kernel: verifying traces from MIR down

Part 1 of the content-keyed incremental kernel. Lane pure checks that a step is pure, lane
identity gives the stable identities and code identities, and [verify](verify.md)
checks the store's hits. The code is `crates/blaze-kernel`.

## What it is

A compute step is a registered, pure function of its input. It reads the world only through
a recording context, `Reads`. Each read is narrow (the layout of `Foo`, not `Foo`), is named
by stable bytes (a kind and a key: content IDs and item keys, never a dense id or a path in
the checkout), and is recorded with the hash it saw. The value is kept in the
[store](store.md) as an entry:

    identity (step, of, code, flags) -> { reads: [(read, hash)], value }

A lookup re-hashes each recorded read now and takes the entry only when every hash is the
same. That is a verifying trace (Mokhov, Mitchell and Peyton Jones, "Build systems à la
carte", 2018).

- `Identity { step: StepId, of, code: CodeId, flags }`:
  - `of` is the stable identity of what is computed, from lane identity.
  - `code` is the step's code identity: its `compute` and everything that calls, also from
    lane identity.
  - `flags` is everything else outside the reads that changes the value (target, settings).
- Two values under one identity and set of reads are refused (`KernelError::Conflict`, rule
  18), and the first value stays. Either the identity misses an input or the step is not
  deterministic, so the fix is the key or the step, never the check.
- A run whose read failed keeps nothing, even if the step swallowed the error, because the
  context remembers the failure. So does a run in which one read saw two hashes
  (`KernelError::Unsteady`: the world changed under the step).
- An entry's reads are sorted by kind and key, each once, so read order, maps and threads
  never reach the key (rule 22).

## Beside blaze-query, under codegen

The front end keeps its engine. `blaze_query::db` is derived from salsa: dense ids, in
memory, one view at a time, rebuilt or restored with the workspace (lane warm-restart). The
kernel is for what is worth keeping across processes and checkouts, from MIR down:

- codegen of a function instance,
- later, release templates, summaries, statics, folded bodies, const eval and layouts.

A kernel step reads the front end through a `World` that its wiring crate implements:

- `World::hash(read)` answers a read by its stable key, e.g. a `blaze_items::Fact` resolved
  back to a dense id with `blaze_items::key::resolve`.
- `World::describe(read)` names the read for errors and checks.
- A typed `Fact` gives the value and its hash in one call. The kernel's contract is that
  `World::hash` equals the hash `Fact::get` gives.

The kernel does not depend on `blaze-query`. The crate root's content-keyed engine in
blaze-query (`Database`, `Query::PRODUCER`, memos verified per view) stays for blaze-units and
blaze-tree. Its persisted memos could later be filed as kernel entries.

## In the store

Every value is an ordinary blaze-cas value, so it is restored as the store is, with nothing
of its own to persist (rule 23).

| what | key | bytes |
|---|---|---|
| entry with no reads | `whole::key(identity)` = `H("blaze v2 kernel entry 1", id, 0)` | the bare value |
| shape slot `n` | `H("blaze v2 kernel shape slot 1", id, n)` | magic, id, each read's kind and key (no hashes) |
| entry with reads | `H("blaze v2 kernel entry 1", id, count, each (kind, key, seen))` | magic, step, id, each (kind, seen, hash, key), the value at a 16-byte boundary |

- **Lookup** probes the entry with no reads first (one probe: a step whose identity is its
  whole content costs nothing more). Otherwise it walks the shape slots from 0. For each
  shape it hashes the reads now and probes the entry those hashes name. An empty slot ends
  the walk as a miss. The walk is bounded by `MOST_SHAPES` (1024), which ends in
  `KernelError::TooManyShapes` (rule 15).
- **Keep** puts the entry first, then makes sure its shape is in the chain: a slot holding the
  same shape ends the walk, and an empty slot is taken. A slot another writer took between
  the get and the put (a `Mismatch`) moves on to the next slot. The store must read its own
  puts at once, which blaze-cas does through `Store`, `Writer` (`blaze_kernel::Writing`) and
  `Reader` (`blaze_kernel::Reading`).
- `Entry::read` refuses an entry filed under a key its own identity and reads don't name.
- An entry with no reads is the bare value, so a million codegen templates pay no header.
  Shapes exist only for steps that read, and a pure step that reads nothing for one input
  reads nothing for every world, so the two layouts never mix under one identity.

## Purity (lane pure)

`Step` and `Compute` carry `#[blaze::pure]`, so lane pure's check walks every method of every
impl. `Reads::get` and `Reads::hash` carry `#[blaze::pure(trusted = "the kernel records every
read ...")]`: they are the trusted door, and their bodies (the world) are not walked. A step
that reads anything outside `Reads` is refused by the lint, and in a build a sampled check
catches it (below). The trusted functions a step reaches (`PureProof::trusted`) belong in its
`CodeId`; lane identity hashes them in.

## Identity (lane identity)

The kernel takes `Identity::of` and `CodeId` as given. `Kernel::register::<S>(code)` records
a step's `CodeId` once per process, and `Kernel::identity::<S>(input)` puts it in. Until lane
identity gives code identities, codegen passes `Toolchain::id`: the pinned nightly and the
code generator's code identity, of the packages it is built from (`hir_ty_codegen::v2::code`,
`crates/blaze-codegen-code`), never the executable's bytes. The step's numbers live in one table, `blaze_kernel::steps`, and
registration refuses a second step of one number or one name.

## Checks (lane verify)

On a hit that `BLAZE_VERIFY_STORE` samples, `Kernel::get` runs the step again with fresh
reads. It implements `blaze_store_check::Recompute`. The second run must make the entry's reads
and its bytes, and either difference is a `KernelError::Verify` naming the step, the input,
the identity and each read with its hash. Sampling is by `Identity::id`.

## The first step: codegen's dev template

`blaze_codegen::kernel::DevTemplate` is the kernel step of a function instance's dev
template:

- `of` is what the driver keeps the instance's code as: `placed(stored(instance key,
  built_in), refs)`.
- `flags` are its `Settings`, and `code` is `Toolchain::id`.
- Its entry has no reads, so it lives under `whole::key`, and its bytes are the template's
  blob as before.

Every dev template the driver reads or puts goes through `blaze_codegen::kernel::template`:
`driver::dev`, the lazy path in `driver/later.rs` (its own tier and other tiers), and
`blaze_global`'s rule for public dev functions. `query::keys::template` is gone. Existing
stores miss their dev templates once.

The driver still compiles templates in batches (the memo, flights, folding), so
`DevTemplate` is a `Step` the kernel keys, not a `Compute` it runs. Next, in order:

1. Done at package grain (2026-11-02): `Settings` has no executable hash left;
   `Toolchain::id` is the code generator's code identity, so a blaze rebuilt without changes
   to the code generator's packages reads the last one's templates. Next, lane identity gives
   `DevTemplate` the `CodeId` of the codegen code it runs, finer than a package.
2. The instance key's `args` (the facts of the type arguments the code reads) become recorded
   reads of a `World` over `blaze_items` facts, and `of` becomes lane identity's identity of
   the generic. One entry then serves every instantiation whose facts hash alike.
3. Release templates, summaries, statics, folded bodies and thunks become steps.

## Found on the way

- The "store: N functions found, 0 compiled and added" line is the pinned v1 engine's: its
  LLVM per-function cache counts hits and misses, and v1 stores no checkout code once a patch
  base exists (diagnosed by lane warm-restart). The v2 driver puts every compiled template.
- `driver::Out::finish` deduped its puts by key, so a second put of other bytes under one
  key in a build was dropped before the store saw it. It now refuses that pair (rule 18),
  naming both puts (`put_twice`, driver/tests.rs).
- `blaze_global`'s rule for public dev functions keyed a function by `stored(instance,
  built_in)` without `placed(.., refs)`. Since `placed` always rehashes, no key it marked
  public was ever one the driver keeps. `DevFunctionMade` now carries `refs`, and the rule
  keys by `blaze_codegen::kernel::template` of `placed(..)`, as the driver does.
