---
status: plan
summary: speed of light for hir-ty-unsafeck, rustc's unsafety check over a body's store and inference, with the arithmetic and what the code does to reach it.
date: 2026-10-29
---
# Speed of light: unsafety checking (`crates/hir-ty-unsafeck/`)

Nothing here was run: costs are counted from the code, with the unit costs of
[speed-of-light-infer-relate.md](speed-of-light-infer-relate.md) (an L1 hit is about 3 cycles,
an L2 hit about 16, and a salsa memo hit about 100).

## The work

`unsafety_check(def)` is one walk over the body's E expressions and P patterns in THIR's order,
plus one walk of each repeat count. It is a tracked query with a `Hash`/`Eq` result, so an edit
that leaves a body's operations as they were is cut off there.

The work cannot skip a body. A "may contain unsafe" bit per body would not help, because any call
can be unsafe through its callee, and nearly every body has a call. The floor is therefore:
- one decode of each node's kind from the store's columns, about 5 cycles;
- one side-table read where an operation can be: a field resolution per field access, the
  adjusted type per call, a method resolution per method call, the binding mode per binding
  inside a layout-constrained pattern, and the first adjustment per field or index;
- one signature lookup per distinct callee, `FunctionSignature::of`, a memo hit of about 100
  cycles.

For a body of E = 1000 with C = 100 calls, that is about 5,000 + 2,000 + 10,000 = 17,000 cycles,
about 5 µs at 3.5 GHz. The callee lookups dominate.

## How the code gets there

- `UnsafeOp` is 8 bytes (`const` assertion): a packed node, `u8` reason and lint enums, and two
  flags. The result is one boxed slice. A body with no operation allocates nothing, since
  `Vec::new` and an empty `into_boxed_slice` do not allocate.
- Fixed sets are enums with a `u8` repr (`UnsafetyReason`, `UnsafeLint`), and the target
  features are `hir-ty`'s `u16` bitsets, so checking a `#[target_feature]` call is a subset test.
- One-entry caches cover the last variant's fields, the last ADT's layout attribute and the
  `Drop` trait, because field accesses and patterns run on one type at a time.
- The inference context for `Freeze` is made lazily, at the first shared borrow of a
  layout-constrained place, which almost no body has.
- There is no `dyn` in the walk. `Sink` is a generic, so the query and the editor's callback
  each get their own copy of the walker.

## Static paths

`check_static` does not resolve anything: inference already records what each value path
resolved to (`InferenceResult::value_resolution`), so the check is one column read per path
plus a `StaticSignature` read for the few that named a static. A scope walk through the
resolver would cost about 20 to 50 cycles per use of a local, well above the floor's 5.
