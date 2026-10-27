# Item identity: Rust items by content, after Unison

2026-10-25. For [roadmap.md](../roadmap.md) section 1b ("Items by content") and
[#344](https://github.com/indexable-inc/blaze/issues/344). Each claim is
marked **read** (in a source we read, or measured by us), **reported** (a document says so, not
re-checked) or **unverified** (our inference). The Unison pages are in
[upstream/unison/](../upstream/unison/SOURCE.md); den's documents are read in place at
`/Volumes/Projects/andrewgazelka/den/docs/` (`structure.md`, `build.md`, 2026-10-25).

## What Unison does

- A definition's identity is a hash of its syntax tree without names: "all named arguments are
  replaced by positionally-numbered variable references, and all dependencies [...] are replaced
  by their hashes", and names are "separately stored metadata" pointing at hashes (**read**,
  the-big-idea.md). The hash is SHA3-512 (**read**, hashes.md).
- A dependency's hash is its whole hash, so a definition's hash "pins down all its
  dependencies" (**read**, the-big-idea.md): editing a callee's body gives every caller,
  transitively, a new hash. Unison caches typechecking by it and never invalidates (**read**).
- A cycle of mutually recursive definitions hashes as one: a member is `#x.n`, `x` the cycle's
  hash, `n` its index in "a canonical order determined by sorting all the members of the cycle
  by their individual hashes (with the cycle removed)" (**read**, hashes.md). The page does not
  say how two members with the same cycle-removed hash are ordered (**read**: it is not there).
- Two types of the same structure are the same type unless declared `unique`, which puts a
  generated identifier into the hash (**read**, unique-types.md). Content is not identity: the
  language has to add identity back where it means something.
- The v2 codebase is SQLite. A component (a whole cycle) is one object under one hash, and each
  definition in it carries a lookup array of the text and object ids it refers to, so "when a
  definition is transferred between codebases, only the lookup array is rewritten" (**read**,
  repoformat-v2.md).

## What den decided

- **Ids in storage, hashes derived.** den does not store Unison's version-hash references: one
  edit to a widely used function would rewrite hundreds of definitions and bring back conflicts
  between agents, so storage refers by `DefId` and hashes are derived (**read**, structure.md
  section 2).
- **Three identities.** `sid` (den's IR closed over references, no toolchain, for history and
  merge), `hid(def, T, target)` (the compiler's resolved, expanded, type-checked hash, for
  build keys), `bk` (the REAPI action digest) (**read**, structure.md section 2, build.md 3a).
- **`sid` alone is not a sound build key**: "adding an `impl` in another definition changes
  what `x.method()` resolves to without changing the caller's `Ref`s, and macro expansion hides
  edges; those are what `hid` sees" (**read**, structure.md section 2).
- **Names.** Function, const, static and module renames are free; ADT, trait, field, variant,
  method and associated item names are content, because "a type's name shows through
  `derive(Debug)`, `type_name`, serde", a reason den itself marks unverified (**read**,
  structure.md section 2).
- **`hid` comes from loom's hash-rustc**: HIR after resolution, expansion and typeck, locals by
  binder position, SCCs by partition refinement, references replaced by item hashes (**reported**
  by structure.md section 1). It "deliberately omits selected trait implementations, implicit
  adjustments, and optimization hints", so loom added lowered-code keys for objects
  (**reported**, build.md 3g). Per-definition hashes "give exact cones but not cutoff; an
  interface-only fingerprint is a new addition" (**read**, structure.md 4a); cutoff in rustc is
  blocked by the crate hash in metadata (**reported**, build.md 3g).
- **Refusals are data.** An item without an identity says why, and coverage is a number in
  every report (`den-identity/1`) (**read**, build.md 3l).

## What blaze keys on today

- A compiled function's key is a SHA-256 over backend, optimisation level, target, debug info,
  overflow checks and its stable name; a crate of the checkout is named by a hash of its source,
  so "an edit gives new names to everything in the edited crate and in the crates of the
  checkout that depend on it" (**read**, docs/cache.md). In the replays below that is every
  key of every crate counted (**read**, by construction).
- Inside one daemon, an item is known by its file and its place in the file's table of items
  (`ErasedFileAstId`), and an entry's mark hashes its code and what it calls and reads by those
  names (**read**, cache.md; `hir-ty-codegen/stable.rs`, `patch.rs`).
- `line!()`, `column!()`, `file!()` and panic locations already reach code as data requests
  filled in by the code generator, not as bytes of the code (**read**, `builtin_location` and
  `DataRequest::Location` in `hir-ty-codegen`).
- Neither backend inlines one function into another in a build (**read**, docs/support.md);
  the tier-up at level 2 and `RA_JIT_INLINE` builds do (**read**, docs/tiers.md, cargo.md).

## What follows for Rust

1. **MIR is the form** (**unverified** as complete, **read** for what it holds). rust-analyzer's
   MIR is after macro expansion, `cfg`, name resolution, inference, method and impl selection
   for the body's own calls (the callee and its generic arguments are in the operand), and
   adjustments (lowered to statements). Hashing it puts the results of those decisions in the
   hash; what remains is to recompute them when their inputs change, which is invalidation, not
   hashing.
2. **Four hashes, not one** (**unverified** design). Unison's and hash-rustc's hash pins the
   bodies of dependencies, which gives exact cones and no cutoff. A function's code compiled by
   blaze depends on its callees' signatures, not their bodies (point 4 above), so the key of its
   code takes its callees' interfaces, and a callee's body edit leaves its callers' code keys
   alone. The deep hash (Unison's) stays for what does depend on everything reachable: test
   results, and code built with bodies inlined into it.
3. **Content is a key, never an identity** (**read** for the fixture: two equal statics share
   one deep hash). `struct Meters(f64)` and `struct Feet(f64)` hash alike, as Unison's
   structural types do. Wherever Rust observes which item it is (a `TypeId`, `type_name`, which
   impl a trait call selects, a static's address, a vtable) the stored code refers to it by
   slot, and the slots are bound by the root that loads the code: Unison's lookup arrays, with
   the root's items in place of object ids.
4. **What is decided outside the item.** Facts that depend on other items than the ones the
   form refers to: whether a type needs drop (any `Drop` impl), auto traits, the impl a generic
   body's obligation selects once its type arguments are known, associated types and constants
   through impls, layouts. These go into the key of the instance that asked, as answers, and are
   checked again on a hit: a verifying trace in the sense of "build systems à la carte" (Shake's
   verifying traces, **reported**, as den build.md section 2 cites it).
5. **Observers of place and name** are inputs of their own: a move must not change the code.
   `module_path!()` expands to a string literal today and so is in the form (**read**);
   `type_name` and `TypeId` are answers bound by the root; symbol names come from the name map at
   link time.

## Measured

On the fixture of `crates/blaze-items` (19 items: renames of a function, a struct and locals,
two functions moved to another file, items reordered, rustfmt, comments, a `;` added inside a
`macro_rules!`; debug build on the Mac): code 100% unchanged, deep and interface 94.7% (only
the macro, whose tokens changed), today's names 68.4%, today's store keys 0% (**read**). A body
edit of `add` changes only `add`'s code and deep hashes and its caller's deep hash; the
caller's code hash is kept (**read**).

The replays of real commits are in roadmap.md section 1b.
