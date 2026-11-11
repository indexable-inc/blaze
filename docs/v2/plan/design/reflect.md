---
status: plan
summary: Twelve things built as libraries on compile-time reflection (keys that cannot miss a field, checked persisted layouts, reachability assertions, state-keeping hot reload, pure functions as cached queries, record and replay, contracts as property tests, derived layouts, tree operations, memory by type, one outward schema, UI from types), where each lives, what it waits on and the order.
date: 2026-11-09
---
# Built on reflection

[comptime](comptime.md) gives code a type's fields at compile time. This page lists what blaze
builds on it. Each is a library in [bstd](std.md) (`bstd::reflect` unless a home is named), not a
compiler feature (rule `library-over-language`); several turn a rule that review enforces today
into a compile error.

Where reflection stands: phase 1 (`core::mem::type_info`, a type's fields as const data) is
built. Phase 2 (`fields_of`, `value.[f]`, `Field<T>` bounds: looping over a value's fields in
typed code) is partly built. Phase 3 (types made by const code) is design only.

For every item: a compile error names the item and the rule, with a test per error; output is
deterministic (fields in declaration order; build twice, compare bytes).

| # | what | needs | home |
|---|---|---|---|
| 1 | **Keys that cannot miss a field.** A key is derived by walking a type's fields. A field left out is a compile error naming it, unless it is marked excluded with a reason. Enforces rule `keys-cover-every-input` | phase 2 | `bstd::id` |
| 2 | **Persisted layouts checked at compile time.** A schema hash computed from the type (names, types, order) is compared with a committed one: changing a persisted struct without bumping its version is a build error naming the type and the field. The byte-for-byte round-trip test is generated for every persisted type. Enforces `persisted-state-round-trips` | phase 1 | `bstd::record` (which already keys records by a schema hash; this makes the hash derived, not written) |
| 3 | **Reachability assertions.** One const walk, `reaches::<T, Q>()`, answers a question `Q` about what `T` owns; `none::<T, Q>()` is the same for `const _: () = assert!(..)` or a `where const` bound. Questions: `Cycle` (the derived trait `Acyclic`), `AnyCapability` (`NoCapability`), `AnySecret`, `AnyCell` (`Freeze`). The refusal names the field path. Written 2026-11-10: [ownership](ownership.md) | phase 1, `where const` | `bstd::own` |
| 4 | **Hot reload that keeps state.** When a patch changes a struct's layout, live values are migrated by field name from the old definition to the new. A field with no source and no default is a refusal naming it, never a restart or a rebuild | phase 2, the live-patch engine | `blaze-live` |
| 5 | **Any pure function as a cached query.** Purity proves the result depends only on the arguments; item 1 derives the key from the argument types; the key includes the function's code identity. Memoization, invalidation and the remote cache then apply to an ordinary function, with no query macro | item 1, `#[blaze::pure]`, the [kernel](kernel.md) | `blaze-kernel` |
| 6 | **Record, replay and fault injection for every capability.** Done at the boundary, not per capability: every op and event crosses two rings ([std](std.md), "The boundary"), so one recorder captures a real run, one replayer feeds it back, and the simulation already injects failures. A real run's IO becomes a deterministic test | the boundary (S4 of std); no reflection | `bstd::test` |
| 7 | **Every contract is a property test.** Inputs are generated from argument types, pattern types valid by construction; each function with `requires` and `ensures` is called and its `ensures` checked. No test written per function | phase 2, [contracts](contracts.md), Hegel | `bstd::test` |
| 8 | **One struct, several layouts.** From one definition: struct-of-arrays columns, a bit-packed form sized from pattern types, and the Parquet schema. Tracing's encoders are one user | phase 3 | `bstd::record` |
| 9 | **Tree operations with no visitors.** Visit every `X` inside any tree, deep size, deep copy into an allocator, and a structural diff that names the field path where two values differ (used for test failures) | phase 2 | `bstd::reflect` |
| 10 | **Memory by type and field.** Deep size from item 9, reported through the memory profile, so bytes are attributed to what they are and not only to who allocated them | item 9 | `bstd::mem` |
| 11 | **One schema for everything outward.** The agent API's CBOR schema, the CLI's arguments and their docs come from the same types, with a compile-time compatibility check against the previous version's committed schema | phase 2, `bstd::data` | `blaze-api` |
| 12 | **Flecs components and inspector panels from types.** Components registered with their field metadata; an editing panel generated for any struct | phase 2 | the agent app (`crates/harness`), not bstd |

Items 4, 5 and 6 touch durable, cached or concurrent state and are proved by a deterministic
simulation test with an oracle and a planted bug. Items 1 and 5 change cache keys: each states
what its key covers.

## Order

1. **Now, on phase 1:** 2 and 3. They need only a type's shape as const data.
2. **When phase 2's field loop is finished:** 1, then 9 and 10, then 5.
3. **With their other half:** 7 (contracts), 11 (`bstd::data`), 4 (live patching), 6 (the flat
   boundary).
4. **Last:** 8 (phase 3) and 12 (the app's lane).

Finishing phase 2 is therefore the one piece of compiler work most of this page waits on.
