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
it costs nothing outside const bodies. Tests: `crates/hir-ty-mir/src/tests/const_effects.rs`.

Tests: `crates/hir-ty-mir/src/tests/consteval/const_traits.rs` (values from rustc on the same
programs).

## Extensions (beyond the pinned nightly)

Two are planned; v2 has neither yet. rustc refuses the programs, so a workspace that uses them
would build with blaze only, each opt-in by name in the root manifest:

```toml
[workspace.metadata.blaze]
experimental = ["const-hash", "const-fn-pointers"]
```

Today v2 knows only the experiments `sandboxed-tests` and `const-macros`
(`crates/blaze-units/src/load/sandbox.rs`, `KNOWN`) and stops at any other name, so this table
is refused.

- `const-hash`: `core::hash::Hash`, `Hasher` and `BuildHasher` are const traits, so
  `T: [const] Hash`, `const impl Hasher for MyHasher` and `#[derive(Hash)]` types hash in
  const. In the pinned nightly none of the three is const ("`[const]` can only be applied to
  `const` traits"). Hashing in const is what lets a build graph key its steps by a hash
  computed at compile time. The `library/` blaze builds declares them as the nightly does
  (`library/core/src/hash/mod.rs`), so v2 refuses these programs.
- `const-fn-pointers`: calling a `fn` pointer in const. The nightly refuses it ("function
  pointer calls are not allowed in constant functions"), and so does v2's const checker.

blaze checks const bodies against rustc's `check_consts` rules (#914, #1440,
`crates/hir-ty-mir/src/mir/const_check.rs`): E0015 for a non-const call, a call of a `fn`
pointer, an intrinsic not declared `const`, and const stability (`const_check/calls.rs`: the
nightly's atomics are `const` only under `#![feature(const_atomic)]`). Its errors are part of a
unit's check (`crates/hir/src/diagnostics.rs`, `crates/blaze-cargo/src/serve/unit/checks.rs`),
so they fail the build as rustc's do.
