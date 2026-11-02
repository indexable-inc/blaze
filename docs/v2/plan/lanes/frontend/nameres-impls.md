---
status: plan
summary: frontend-nameres-impls — one module's impls as a backdating query (`module_impls`), and the impl buckets of hir-ty and blaze-items built from it module by module, so an impl added anywhere reruns only the buckets it is in (frontend.md items 2, 3).
date: 2026-10-29
---
# frontend-nameres-impls: impl lists per module, buckets per module

[frontend.md](../../design/frontend.md) items 2 and 3, under the design of
[nameres.md](nameres.md) section 9.

## The problem

The readers are per bucket already: hir-ty's `in_crate_for_self_ty`,
`in_crate_for_trait_and_self_ty`, `in_crate_and_deps_for_trait_and_self_ty`,
`blanket_in_crate_and_deps`, blaze-items' `impls(ImplIndexKey)`, `blanket_impls`, `traits_with`.
But every one reads a crate-wide value: `TraitImpls::for_crate` and `InherentImpls::for_crate`
walk `crate_def_map(krate).modules(db)` and each module's whole scope, and blaze-items'
`traits_in` does the same. So any item added anywhere reruns both tables; any impl added
anywhere changes them, which reruns every bucket of the crate, and through
`for_crate_and_deps` every `(trait, self type)` memo of every dependent crate.

## The design

Three layers, each a salsa memo that backdates when equal.

1. **`module_impls(module)`** (hir-def, `nameres/facts/impls.rs`, this part): the module's
   trait impls, inherent impls and builtin-derive impls in declaration order, read from the
   module's final `ModuleScope` (`DefMap::module(db, m).scope(db)`) and split by the scope's own
   bits, which is what hir-ty's tables read. It reruns when the module's scope changes and is
   equal unless an impl comes or goes. A block module has its own (its def map is its own
   query); the crate's tables reach those through the bodies a module holds.
2. **One module's tables** (hir-ty, `TraitImpls::for_module`, `InherentImpls::for_module`):
   the old collection's loop body for one module, reading `module_impls` and the blocks of the
   bodies `impl_holders` keeps. They are what the old walk produced for that module, in its
   order.
3. **Buckets**, each reading only the modules with impls in it: a crate index of the modules
   per trait (per simplified self type), one memo per (crate, trait) (per (crate, type)) for
   that list, one per (module, trait) (per (module, type)) for that module's slice, and the
   bucket `TraitImpls::for_crate_and_trait` / `InherentImpls::in_crate_for_self_ty` merging
   the slices in module order. `in_crate_and_deps_for_trait_and_self_ty` and
   `blanket_in_crate_and_deps` read each loaded crate's `for_crate_and_trait` instead of
   `for_crate_and_deps`. blaze-items' `trait_impls_in` reads `for_crate_and_trait`; its inherent
   index and `traits_in` get the same per-module layers.

`for_crate` stays (the solver's whole-table walks, frozen encoding) as the merge of the module
tables in module order, so every run holds its impls in the order the old walk found them and
the frozen bytes are unchanged. A frozen crate keeps its one frozen table, and its buckets read
it: it never changes.

Why no per-module summary query in hir-def: the bucket keys are a trait and a simplified self
type, which only hir-ty's lowering knows (`impl_trait`, `impl_self_ty`, `simplify_type`). The
summaries are hir-ty's `modules_by_trait` / `modules_by_self_ty`, built from the module tables.

Why `module_impls` reads the scope and not the round ops (nameres.md section 9 said ops): the
frozen interface (`frontend-nameres-shared.md`) makes it read the public `DefMap` API only, so it
is independent of the resolver's internals and works for frozen and block def maps alike. The
cost is that it reruns (and backdates) for every edit of its module, which is one pass over the
module's impl column.

## What reruns, per edit

| edit | reruns | left unexecuted |
|---|---|---|
| an item that is no impl, in module `a` | `module_impls(a)`, `for_module(a)` (both equal) | every other module's, the indexes, every bucket, both `for_crate` |
| an impl of `T` for `X`, in `a`, `a` had impls of `T` | `a`'s, the indexes (equal), `in_module_for_trait(a, T)`, `T`'s bucket in the crate, the `(T, X)` memos, `for_crate` | other traits' buckets, the inherent ones |
| a first impl of `T` in `a` | as above, plus `modules_of_trait(T)` | other traits' `modules_of_trait` and buckets |
| an inherent impl for `X` in `a` | `a`'s, the type index (equal unless first), `in_module_for_self_ty(a, X)`, `X`'s bucket | trait buckets, other types' |

## Speed of light

- `module_impls` on a rerun: one pass over the scope's impl column (8 bytes an impl) and its
  trait bit column (one bit an impl), twice for the bits (count, then fill, so each list is
  allocated once at its exact size), and the equality check that backdates it, which reads the
  old lists once. The floor is that copy and compare, `O(impls in the module)`: for 100 impls
  under a microsecond, all in L1. Within 10% of the floor: the extra count pass reads
  `impls / 64` words.
- A memo hit: one salsa memo lookup and a pointer to the stored value, no allocation (tested).
- Buckets per impl edit: the floor is re-deriving the edited module's table (memo hits for
  `impl_trait` and `impl_self_ty`, `O(impls in the module)`) and the buckets the changed impl is
  in (`O(impls of that trait in the crate)`). The overhead above it is the index rerun, `O(m)`
  memo validations over the crate's modules (about 50 µs for 1000 modules at 50 ns each), once
  per impl edit, not per bucket; buckets of other traits validate in `O(1)` edges each.

## State of the art

- rustc: `trait_impls_of(trait)` is per trait but built from `all_local_trait_impls`, a map
  over the crate's whole HIR, so any item change reruns the map and red-green cutoff saves the
  per-trait results only after they recompute.
- rust-analyzer: `TraitImpls::for_crate` per crate, invalidated by any item change (what blaze
  had).
- Incremental Datalog (differential dataflow arrangements): an index keyed by the join key,
  updated by deltas of its inputs. This design is that index for impls, with modules as the
  units of delta and salsa's backdating as the cutoff at each key.

## What proves it

| claim | test |
|---|---|
| `module_impls` lists a module's impls in order, trait vs inherent right (with `cfg`, a macro expansion, a child file), builtin derive impls for their type | `nameres/facts/tests/impls.rs` `lists_a_modules_impls_in_order` |
| an item added to `a` leaves `module_impls(b)` unexecuted and `a`'s equal; an impl added to `a` reruns `a`'s only | `facts/tests/impls.rs` `reruns_only_for_the_module_edited` (salsa keys and `log_executed`) |
| a memo hit allocates nothing | `facts/tests/impls.rs` `a_memo_hit_allocates_nothing` (`blaze-alloc-count`) |
| the per-module buckets equal the crate tables | blaze-items `tests/narrow.rs` `per_module_buckets_are_the_crate_tables` (requested) |
| an impl reruns only its trait's bucket; a non-impl item reruns no bucket | `narrow.rs` `an_impl_in_one_module_reruns_only_its_trait_bucket`, `an_item_that_is_no_impl_reruns_no_bucket` (requested) |
| frozen tables are unchanged | `frozen::check` in `Mode::Check` over the merged `for_crate` (existing frozen tests) |

The hooks: `v2/requests/db.md` (re-export), `v2/requests/items.md` (the hir-ty and blaze-items
code, tests, hir-ide expectations), `v2/requests/db-ty.md` (query kinds). Index:
`v2/requests/frontend-nameres.md`, `## impls`.
