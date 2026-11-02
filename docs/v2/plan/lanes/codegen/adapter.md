---
status: plan
summary: Speed of the v2 adapter (`crates/hir-ty-codegen/v2*`) - describing an instance, preparing its IR against its references, its shape, and converting its code - against rustc's mono-item collector, Cranelift's incremental cache key and rustc's debuginfo scopes; what it does per instance after the 2026-10-29 pass and what it would take to beat each.
date: 2026-10-29
---
# The adapter's hot paths (codegen-adapter)

Every instance of a dev build goes through four steps here, once each: `describe`
(`v2/frontend.rs`), `prepare` (`v2/backend.rs::prepared`), its shape (`v2/shape.rs`) and, on a
miss, `emit` (`v2/backend.rs::told`, `v2/convert.rs`). With full debug info (the dev profile)
`prepare` also says the scopes of its named locals (`scoped`).

## The benchmarks, and where v2 stands

- **References: rustc's mono-item collector** (`items_of_instance`, a query a revision per
  instance). v2's walk is the same, memoized by salsa per instance (`refs::walked`), and
  `prepare` borrows the memo (`refs::listed`, `Placer<'a>` over a `Cow`): nothing copied.
  Placing a request is one Fx hash and one `hashbrown::HashTable<u32>` probe, checked against
  the list, with no reference cloned into a key; a symbol request is found by its `&str`
  without making the reference (`Placer::symbol`). Matches rustc's `FxIndexSet` of mono items.
- **Dedupe key: Cranelift's incremental cache** (`compute_cache_key`, the stencil's `Hash`
  into SHA-256). v2 streams the same derived `Hash` into one presized buffer with no hasher
  between (equal bytes are equal IR, no collisions), names by place among the references, so
  `Vec<A>::drop` and `Vec<B>::drop` of pointer-sized `A`, `B` are one shape where Cranelift's
  key keeps them two. One pass over the IR, about 4 to 8 bytes a value and instruction.
- **Scopes of locals: rustc's `compute_mir_scopes`** and LLVM's `DILexicalBlock`s, made once
  per function. v2 makes the scope tree once per prepare from the body, looking only for the
  bindings told of (a sorted `SmallVec` probed by binary search, the walk stopping once all are
  found) and walking `match` arms only when a row is of a pattern; the ranges come from the
  line rows at emit, `O(rows x depth)`. `SpanScope` is 8 bytes (`const` asserted).
- **Conversion**: the code's bytes move into the `CompiledFunction` (no copy); requests resolve
  through `places::Placed`, static dispatch (`convert::Resolver`), no `dyn` a relocation;
  symbols indexed on first use, one probe each after. Line rows are preallocated.
- **Side table**: names are the body's interned `Symbol`s (a `Copy` pointer, no `String` a
  local); the table borrows them when written and borrows them from the stored bytes when read
  (postcard, zero-copy `&str`). `side::Variable` is 64 bytes (`const` asserted).

- **Built-in callees: LLVM's always-inliner at -O0.** A v2 unit builds in the
  `#[inline(always)]` callees of crates no edit reaches, as cargo's debug build does. The walk
  lists the built-in body's requests from the memoized body (`declare_inlined`), walking no
  callee again. The code hashes are gathered only for an instance that builds something in,
  so nearly every instance allocates nothing for them.
- **One walk for every unit.** The walk reads level-1 bodies with unwinding and debug
  assertions on, which ask for a superset of every other setting. So `describe` is spec-free and
  memoized once per instance, where rustc collects once per session and level.

## Where v2 is behind, and what it would take

- `describe` copies the memoized walk into `Described::refs` and each outside symbol into a
  `String` (`FrontRef::Extern`, `Described::symbol`), and `resolve` clones a `Target::Extern`
  `String` a relocation. Fix: interned symbols in `blaze-codegen`'s `FrontRef`/`Target` and
  `Described::refs` borrowed from the front end (requested of codegen).
- The IR builder runs twice per instance: once for the walk (`refs::scratch`), once in
  `prepare`. rustc collects from the MIR it already built. Fix: one build per instance whose
  requests are the walk, kept until `prepare` (needs `describe` and `prepare` to share the
  build, a parent interface change).
- `Homes` (which crate an instance is compiled as) rebuilds its per-crate sets each call now
  that the database is shared with no per-thread state. Fix: what a crate sees as a salsa
  query (requested of devspeed, `home.rs`).
- `SlotPlaces::of` finds a slot by a linear scan, so the side table is `O(locals x slots)`.
  Fix: slots sorted by number and found by binary search, or indexed (`debuginfo.rs`,
  requested).
