---
status: current
summary: Const evaluation: blaze's own interpreter for constants and const fns, and what it covers.
---
# Const

A `const fn` is blaze's pure function: deterministic, touching no mutable state, so its result
can be computed at compile time, cached by its code and arguments, and trusted to be the same
on every machine. blaze evaluates constants with its own interpreter
(`crates/hir-ty-mir/src/mir/eval.rs`) and never asks rustc. Everything the pinned nightly
(`harness/toolchain`) accepts in const is accepted, and gives the value rustc gives; what goes
further is an extension a workspace opts into.

## Const traits, closures and std's const impls

As in the pinned nightly, under `#![feature(const_trait_impl, const_closures, derive_const)]`:

- `const trait Tr { .. }`, `const impl Tr for T { .. }` (the nightly refuses the older
  `impl const Tr for T`; blaze still parses it), bounds `T: [const] Tr` (const where the
  caller is), `T: const Tr` (always const) and the older `~const Tr`, default methods and
  `[const]` supertraits.
- Calls through those bounds in a `const fn`, a `const` item or a `static`, evaluated by the
  impl the type has.
- `#[derive_const(PartialEq, Clone, Default, ..)]`.
- std's const impls: `PartialEq`, `Eq`, `PartialOrd`, `Ord`, `Clone`, `Default`, `From`,
  `Into`, `TryFrom`, `AsRef`, the operator traits (arithmetic, bit, compound assignment,
  `Deref`, `Index`), `Iterator`, `IntoIterator`, `Sum`, `Product`, `Borrow` and `Destruct`.
- `const |x| ..` closures, called in const directly or passed as `impl [const] Fn(..)`,
  `[const] FnMut` and `[const] FnOnce`; a `const fn` item passed the same way.

Whether a call in a const body is const is decided as rustc decides it, by proving the callee's
`[const]` bounds const (`crates/hir-ty/src/effects.rs`): `T: [const] Tr` holds in a const fn
that has the bound, a `const impl` holds for its type when its own `[const]` bounds do, and a
closure or fn item is `[const] Fn` when it is const. `non_const_calls` lists the calls that are
not, for the const checker (#860) to refuse; ordinary type checking does not prove effects, so
it costs nothing outside const bodies. Tests: `crates/hir-ide/src/tests/const_effects.rs`.

Tests: `crates/hir-ide/src/tests/consteval/const_traits.rs` (values from rustc on the same
programs) and the corpus module `const_traits` (`harness/corpus`), which evaluates each of the
above in a `const`, calls the same functions at run time and compares both with rustc's build.

## Extensions (beyond the pinned nightly)

The interpreter runs these. rustc refuses the programs, so a workspace that uses them builds
with blaze only. Each will be opt-in by name in the root manifest:

```toml
[workspace.metadata.blaze]
experimental = ["const-hash", "const-fn-pointers"]
```

- `const-hash`: `core::hash::Hash`, `Hasher` and `BuildHasher` are const traits, so
  `T: [const] Hash`, `const impl Hasher for MyHasher` and `#[derive(Hash)]` types hash in
  const. In the pinned nightly none of the three is const ("`[const]` can only be applied to
  `const` traits"). Hashing in const is what lets a build graph key its steps by a hash
  computed at compile time. The values are those of the same code run at run time, checked
  against rustc's run time (FNV-1a over a derived `Hash` struct, a tuple with `Option`, `str`
  and `char`, a slice, and through a const closure).
- `const-fn-pointers`: calling a `fn` pointer in const. The nightly refuses it ("function
  pointer calls are not allowed in constant functions").

blaze checks const bodies against rustc's `check_consts` rules (#914,
`crates/hir-ty-mir/src/mir/const_check.rs`). That includes E0015 for a non-const call and
rustc's refusal of a call to an intrinsic not declared `const`. Two gaps remain (issue #1440).
The build does not yet fail on those errors: they are IDE diagnostics, so these programs, and
any ordinary function called in a `const`, are accepted without the opt-in. And const stability
is not checked: the nightly's atomics are `const` only under `#![feature(const_atomic)]`, which
blaze does not ask for.
