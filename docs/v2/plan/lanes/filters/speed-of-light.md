---
status: current
summary: Lane filters - where a definite no in front of a membership test skips real work in blaze v2, the one site that pays (trait impls read only from the crates that can name the trait and the type), its bound and bench, and the sites checked and left as they are.
date: 2026-10-30
---

# Filters: speed of light

Lane filters (2026-10-30) puts a "definitely not" test in front of membership tests whose miss
does real work. One site passed the bar: the walks over a crate's loaded crates for one trait.

## The impls of a trait for a type over the loaded crates

`TraitImpls::in_crate_and_deps_for_trait_and_self_ty`, `blanket_in_crate_and_deps` and
`in_crate_and_deps_for_trait` (`crates/hir-ty/src/method_resolution.rs`) read one memo per
loaded crate, `for_crate_and_trait(dep, trait)`: a salsa key interned, a memo found and
verified, and on a cold ask a table built, about 100 ns warm and microseconds cold per crate.
A crate on 300 dependencies pays 300 of them for each (trait, self type) the solver asks, and
the outer memo keeps 300 dependency edges to verify in every later revision.

**Bound.** An ask can do no less than read the tables of the crates that hold one of its impls.
A crate holds `impl T for X` only if it can name both, that is if it is, or depends on, the
crates of `T` and of `X`. So the floor is one memo read per such crate, plus finding them.

**What it does.** `crates_that_name(krate)` is a bit matrix: one row per crate in the loaded
crates' closure, a bit per loaded crate that can name it. An ask ANDs the trait crate's row with
the self type's row (`L / 64` words, 5 for 300 crates) and reads the memos of the set bits,
in `loaded_crates` order. The overhead over the bound is two binary searches over the
closure (9 compares for 400 crates) and the AND, under 50 ns, against the 100 ns or more of
each memo read it saves. For a type of the crate being checked, or of a leaf dependency,
that leaves one or two crates out of hundreds. A trait and a type both of core still read
every crate, which is the bound: each crate could hold such an impl.

The matrix is built once per crate per change to the crate graph or to `loaded_crates`
(which backdates when it is equal): one depth-first walk of the closure and an OR of rows, `K²/64`
words for `K` crates in the closure, under 0.2 ms for 400.

It is exact, not probabilistic: no false positives, no false negatives. A Bloom or fuse filter
of each crate's (trait, type) pairs would cost about the same as the memo read it guards.

**Bench:** `crates/hir-ty/benches/impls.rs` asks 402 (trait, type) pairs of a crate on 200
dependencies, cold, both ways, and prints the ratio and whether it gains.

## Sites checked and left as they are

- blaze-link's archive search (`native.rs` `search`): one in-memory table probe per searched
  archive. A filter probe costs as much as a miss in a half-full table. A merged table would
  only pay with many archives, and native links have few.
- hir-def's name resolution: glob imports are copied into the importing scope when they are
  collected, so a missing name costs one `ItemScope` index probe per scope in a fixed chain.
- Test selection (`serve/unit/reach.rs`, blaze-testsel): reached lists are precomputed. A hit
  is a probe of an `FxHashSet` small enough to stay in L1, and blaze-testsel already has an
  inverted index.
- blaze-vfs and module files: the tree and `FileSet` answer in memory, and `foo.rs` against
  `foo/mod.rs` is two hash probes.
- Build-script and proc-macro caches (`blaze-usercode/src/index.rs`): a chain of CAS gets that
  ends at the first missing shape, each one probe of the mapped index.
- The remote store and the local CAS index were assigned elsewhere (integrate-store's `Fuse8`
  in `crates/blaze-ids/src/fuse.rs`).
- The linker's library search (`blaze-link/src/target.rs` `Line::find`) stats each candidate
  file. That is a negative-probe cost, but it is fixed by listing each directory, not by a
  filter: queued to the link lane (01165).
