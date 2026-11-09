---
status: plan
summary: Field order by heat (lane fieldorder, 2026-11-02) - the fields a profile shows hot code reading laid out first in `repr(Rust)` structs and enum variants, only when the type keeps rustc's size, alignment, ABI, niche and tag; the Reference text that allows it, what is never reordered, the heat estimate from built MIR weighted by PGO counts, the plan input and its place in layout identity, the tests, the benchmark, and the driver hook still to add.
date: 2026-11-02
---
# Field order by heat

rustc orders a `repr(Rust)` struct's fields by alignment, so two fields a hot loop reads
together can end up a cache line apart: `Row { key: [u32; 4], payload: [u64; 21], weight: u32 }`
gets `key` at 0 and `weight` at 184, two lines per visit. With a field order plan, blaze lays
the type out with its hot fields first (`weight` at 16, beside `key`), as long as nothing
outside the type can tell: its size, alignment, ABI, niche and tag stay rustc's. Without a
plan every layout is rustc's, byte for byte, and no layout hash moves.

Code: `crates/hir-ty/src/layout/field_order.rs` (the plan input, the ranks, the reorder),
`field_order/heat.rs` (heat from MIR), the hook in `layout/adt.rs` (`layout_of_adt`),
`crates/blaze-items/src/field_order.rs` (the plan file) and the layout hash in
`crates/blaze-items/src/hash.rs`.

## What allows it

The Reference, [Type layout](https://doc.rust-lang.org/reference/type-layout.html):

- "Type layout can be changed with each compilation. Instead of trying to document exactly
  what is done, we only document what is guaranteed today."
- Of the `Rust` representation: "The only data layout guarantees made by this representation
  are those required for soundness. These are: 1. The offset of a field is divisible by that
  field's alignment. 2. The alignment of the type is at least the maximum alignment of its
  fields." and "For structs, it is further guaranteed that the fields do not overlap. [...] The
  ordering does not have to be the same as the order in which the fields are specified in the
  declaration of the type." and "There are no other guarantees of data layout made by this
  representation."

rustc uses the same freedom: it sorts by alignment and shuffles under `-Zrandomize-layout`.

What programs may rely on, and this keeps:

- `repr(C)`, the primitive representations (`repr(u8)` and the rest, on enums) and
  `repr(transparent)` specify the layout. They are never reordered.
- `align` and `packed`: the Reference says "On their own, `align` and `packed` do not provide
  guarantees about the order of fields", but they are left alone anyway (the lane's brief, and
  `packed` changes what an unaligned field access means).
- [`std::option`'s representation](https://doc.rust-lang.org/std/option/index.html#representation):
  "Rust guarantees to optimize the following types T such that Option<T> has the same size,
  alignment, and function call ABI as T", for `Box<U>`, `&U`, `&mut U`, `fn`, `num::NonZero*`,
  `ptr::NonNull<U>` and "`#[repr(transparent)]` struct around one of the types in this list".
  `Box` and `Option` are lang items, and `NonZero` and `NonNull` are `repr(transparent)`, so all
  are refused. A variant of one field has nothing to reorder, so `Some(T)`, `Ok(T)` and
  `Err(E)` never move. And every reordered type keeps rustc's niche (`largest_niche`, offset
  included), so `Option<T>` of it is laid out as before.

## What is never reordered

`refusal` (field_order.rs), each named on the `field_order` span:

- unions (their fields share offset 0)
- `repr(C)`, a primitive repr, `simd` or `scalable`, `linear`, `transparent`, `packed`, `align`
- `-Zrandomize-layout` types
- `#[blaze::bits]` types
- lang items and the types the compiler reads by itself: `PhantomData`, `Box`, `ManuallyDrop`,
  `UnsafeCell`, `UnsafePinned`, the covariant cell, `f16b`, `ppc_f128`, field-representing
  types, and anything with `#[lang]`
- a type whose last field may be unsized (`T<dyn Trait>` must keep `T<U>`'s order)
- a type whose niches are hidden
- uninhabited types
- a type of at most `LINE` (64) bytes, which fits one line in rustc's order anyway

## How a layout is chosen

`hot_first`. rustc's layout is computed first, exactly as before. Then, for `h` from the most
hot fields of a variant that share a line down to 1:

1. Each variant's order starts as rustc's memory order.
   - If the variant holds the niche other types read: when the niche's field is first, the
     fields up to and including it stay first; otherwise the fields from it on stay last.
   - A direct tag's bytes come before the variant's fields.
   - Next come the variant's `h` hottest fields, in rustc's relative order (alignment first,
     so no padding between them). Each gap an alignment leaves before one is filled with cold
     fields that fit.
   - The rest of the cold fields follow in rustc's order.
   - A variant with no hot field keeps rustc's order.
2. The fields that share a line are the hottest, and those after it while their sizes add up
   to at most `LINE`. A big lukewarm field (one a cold path reads, within `HOT_RATIO` of the
   hot loop) is left cold rather than put before a small hot one.
3. An `h` whose order is rustc's already is skipped.
4. The calculator lays out the permuted fields with `ReprFlags::IS_LINEAR`. In rustc_abi that
   flag only enters `FIELD_ORDER_UNOPTIMIZABLE` (no sorting) and randomization, never the ABI
   or the enum layout optimisations. Then `unpermute` (and rustc_abi's `unpermute_fields`)
   renames the fields back to declaration order.
5. The first candidate whose `same_outside(rustc, candidate)` holds is kept. That check
   compares size, alignment, `backend_repr`, `largest_niche`, `uninhabited`, `max_repr_align`,
   `unadjusted_abi_align`, `repr_c`, `randomization_seed`, and for an enum the tag, its
   encoding (niche variants and start included), the tag field and the top-level field
   offsets. So the type never grows, and every type around it and every call keeps its
   layout. If no candidate passes, rustc's layout stays.

At most `MAX_HOT` (16) extra calculator runs per type and instance, cached by the layout
query. The span records what happened: `applied: N hot fields first, T layouts tried`, or
`not applied: <why>`.

## Heat

`Heat::add_body(db, body, weight)` reads the body's built MIR (`MirStage::Built`) and that of
the closures in it.

- `weight` is what the profile counts for the function: the sum of its record's counters.
- It is spread over blocks by a static estimate: `LOOP_FACTOR` (32) to the power of the loops
  around the block, at most `MAX_DEPTH` (4) levels. Loops are found by an iterative Tarjan,
  level by level; each loop's header is the block a depth-first walk reaches first. Cleanup
  blocks and unreachable blocks get 0.
- Each `Field` projection of a struct place, or of an enum variant place (`Downcast`), adds the
  block's share. This covers statements, rvalues, operands and terminators.
- Building a value (`Aggregate`) is no access.
- Bodies over `BODY_FUEL` (4096) blocks and bodies whose MIR does not build are counted
  (`too_big`, `unlowered`), not walked.

`Heat::ranked` names each variant's hot fields: those whose heat is at least the variant's
hottest over `HOT_RATIO` (32), hottest first, declaration order between equals.

## The plan and the keys

- `FieldOrder` is a singleton salsa input holding `Option<Arc<FieldOrderPlan>>` at high
  durability. `install` sets it only when the plan's ID changed.
- `field_ranks(db, adt)` (tracked) resolves the plan's field names to ids. Its content ID
  (domain `blaze v2 field order 1`) covers the variant count and each variant's ranked ids.
- A plan that names a field the type does not have, a field twice, or the wrong number of
  variants is `Ranks::Stale`, and the type keeps rustc's order. `decode` checks every ADT of a
  plan with `field_order::stale` and returns the stale ones in `Decoded::stale`, naming each,
  so the driver reports them with the build, as it reports profile records that don't match.
- Rule 18: the layout query reads `field_ranks`, so salsa recomputes a planned type's layout
  when the plan changes. `order_id(db, adt)` feeds the type's layout identity
  (`blaze_items::hash`, `Graph::Layout`'s own hash) only when the plan ranks the type, so a
  build without a plan keeps every layout hash. Code whose identity covers a type's layout
  gets the plan through it.
- The plan file (`blaze_items::field_order`): a header line `blaze v2 field order 1`, then
  `adt <item key, 64 hex> <variant count>` and `v <variant index> <field>...` lines, sorted by
  item key. The plan's ID is the content ID of the bytes. `decode` refuses malformed lines,
  non-ADT keys and an ADT named twice, naming the line, and counts keys this database does
  not have (`unresolved`).
- Rule 22: `encode` sorts by item key, `ranked` breaks ties by declaration order, and
  `hot_first` is a function of the ranks and rustc's layout alone.

## Not done: the driver hook

Nothing installs a plan in a real build yet. blaze-cargo's `serve/unit` files carry other
lanes' open edits, so this lane left them alone.

1. **Produce.** In a job whose units have `-C profile-use=PATH` (`serve/unit/codegen.rs`,
   `profiles`):
   - match the profile to functions as the profile lints do (`blaze_lints::profile`,
     `Hits::of(FunctionId)`; `hits_of` is `pub(crate)` today and needs a public entry)
   - call `Heat::add_body(db, InferBodyId::from(f), hit.count)` for each function with a hit
   - then `heat.ranked(db)`, `blaze_items::field_order::encode`, `decode`, and
     `field_order::install(db, Some(Arc::new(decoded.plan)))`, before any unit's codegen
   - report `decoded.unresolved`, `decoded.stale`, `heat.unlowered` and `heat.too_big`
   - a job with no profile installs `None`
2. **Key.** Put the plan's ID in each unit's record key (`RecordInputs`) whenever a plan is
   installed. Coarse, but it covers every input. Per-item code identity already gets it
   through the layout hash.
3. **A/B switch.** Give an opt-out a unit's flags can carry, so a with-profile build can be
   measured with and without field order, and kept off by anyone who needs rustc's order
   under a profile. Proposed: a blaze-only `-Z field-order=off` parsed next to `pgo()`,
   which is allowed as an opt-in extension under `superset-of-rust`. It must be in the
   record key.
4. **Host units.** The plan is per database, so build scripts and proc macros built in the
   same job see it too. Each program's layouts stay consistent within itself, which is all a
   program can observe.

## Tests

In `crates/hir-ty/src/layout/field_order/tests.rs`, the real calculator as `layout_of_adt`
calls it:

- `a_hot_field_declared_last_is_laid_out_first`
- `a_big_lukewarm_field_does_not_keep_hot_ones_apart` (the benchmark's `Row`)
- `hot_first_never_changes_what_other_types_see` (600 random structs and enums: a
  permutation, aligned, no overlap, `same_outside`)
- `the_same_ranks_give_the_same_layout`

In `crates/hir-ty-mir/src/tests/layout/field_order.rs`, programs through the plan input and
the layout query:

- `without_a_plan_the_layout_is_rustcs`
- `a_hot_field_declared_last_is_laid_out_first`
- `the_same_plan_gives_the_same_layout`
- `types_a_program_can_rely_on_keep_rustcs_order`
- `types_around_a_reordered_one_keep_their_layout` (`Option<T>` and a struct holding it)
- `heat_ranks_the_field_a_hot_loop_reads`
- `a_program_reading_every_field_behaves_the_same` (the MIR interpreter prints every field
  of a reordered struct and enum, the same as without the plan)

## Benchmark

`harness/apps/fieldorder` (apps.json entry `fieldorder`) is a table of `Row`s visited at
random. The loop reads `key` and `weight`, and a cold path reads `payload`.

- rustc's order: two cache lines per visit, on 64-byte and 128-byte lines alike.
- With the plan: one line.
- Its output (sums and `size_of::<Row>()`) is the same under any layout, so builds with and
  without a plan compare byte for byte. Its `runs` are small, since `harness/v2/reachable.py`
  runs every local app in dev and release.
- `bench` is `20 50000000`: 192 MiB of rows, far past the last-level cache.

Once the driver hook lands, measured on one exact tree (rule 19: the source digest and the
blaze binary's hash, kept):

```sh
cd harness/apps/fieldorder
# A profile. blaze can't instrument yet (`-C profile-generate` is refused: its linker writes
# no profile sections), so the pinned nightly makes the training profile, as tools/pgo does.
RUSTFLAGS=-Cprofile-generate=/tmp/fo-raw cargo +"$(cat ../../toolchain)" run --release -- 16 2000000
llvm-profdata merge -o /tmp/fo.profdata /tmp/fo-raw
# The same profile, without and with field order.
RUSTFLAGS="-Cprofile-use=/tmp/fo.profdata -Zfield-order=off" blaze build --release && cp target/release/fieldorder /tmp/fo-a
RUSTFLAGS="-Cprofile-use=/tmp/fo.profdata" blaze build --release && cp target/release/fieldorder /tmp/fo-b
/tmp/fo-a 20 50000000; /tmp/fo-b 20 50000000   # the same line
hyperfine -w 2 -r 10 '/tmp/fo-a 20 50000000' '/tmp/fo-b 20 50000000'
```

The B build's `field_order` span for `Row` must say `applied: 2 hot fields first`
(`<BLAZE_HOME>/v2/profile/*.spans.parquet`).

## Known gaps

- Ranks are per ADT, not per instance: `Foo<A>` and `Foo<B>` get the same ranks, each
  validated on its own.
- Block weight is a static loop estimate scaled by the function's total count. The profile
  has per-block counters, but nothing maps LLVM's blocks back to MIR's.
- The niche's field never moves, and a type whose niche sits mid-struct can only reorder the
  fields before it.
- Heat does not see a struct named through an unnormalized alias, or the operands of an
  `Assert`'s message.
- Closures are weighted as the body they are written in.
- `LINE` is 64. On 128-byte lines the "fits one line" refusal is conservative.
- Not built or run yet: the tests above, the benchmark, and std built under a plan (its code
  identity covers its types' layouts through `Graph::Layout`, untested).
