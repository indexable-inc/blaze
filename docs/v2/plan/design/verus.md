---
status: record
summary: The spec form blaze kept from lane verus (Verus's clause syntax as native grammar, gated by `#![feature(blaze_verify)]`, lowered to clause groups in hir-def), and the record of the Z3 tier that proved it until 2026-11-10, when it was retired for Lean.
date: 2026-11-10
---
# The spec form, and the retired Verus tier

Andrew decided the spec form on 2026-11-02: there is one, Verus's own syntax, as blaze grammar,
with no `verus!` wrapper. That stands. On 2026-11-10 he decided that Lean is the prover
([formal-verification.md](formal-verification.md)) and the Verus tier is retired now, not when
Lean reaches parity with it.

## What stays: the spec form

- **Grammar.** `crates/parser/src/grammar/verify.rs` parses five things:
  - `requires`, `recommends`, `ensures` and `decreases` on a function;
  - a named return, `-> (r: T)`;
  - `invariant`, `ensures` and `decreases` on a `loop` or `while`.
  The syntax follows verus-analyzer's grammar.
- **Gate and lowering.** A crate with `#![feature(blaze_verify)]` gets these, and
  `hir-def/src/expr_store/lower/spec.rs` lowers each one. A crate without the feature gets the
  diagnostic `VerifyRefused` for each clause. So does a function that also has nightly's
  `#[core::contracts::*]` markers, and so does a function with no body.
- **Clause groups.** Each clause becomes one ghost clause group at the head of its block. An
  `ensures` group is a closure over the named return. MIR building leaves the groups out, so
  they never run, never borrow and never reach codegen.

The clauses are read from the groups by `crates/hir-ty-lean` (their statements in Lean) and by
`crates/hir-ty-contracts` (the facts the optimizer may use once they are proved). The one
`proved` interface is `hir_ty_lean::proved`:

```rust
pub fn proved(db: &Db, func: FunctionId, args: StoredGenericArgs, clause: Clause, proofs: &Proofs)
    -> Result<ProofKey, NotProved>;
pub enum Clause { Requires, Ensures }
pub struct ProofKey { pub how: Prover, pub id: ContentId }
```

`#[core::contracts::*]` is not a blaze spec form. It stays only because rule 4 needs every
nightly program to build; the `native_contracts` lint rewrites it to the native clauses.

## What was retired (2026-11-10)

- `crates/hir-ty-verus`: the lowering of one function from HIR to Verus's VIR, VIR's checks and
  passes, the AIR encoding and the Z3 driver, with its `proved`, `ProofKey`, `Verdict` and
  counterexamples.
- `vendor/verus` (`air`, `vir`, `vir_macros`, `builtin`, `vstd` at commit `3881d449`) and
  `tools/v2/verus_strip.py`.
- The pinned Z3 4.16.0 and its setting, `BLAZE_Z3`, in the flake and in the daemon's settings.
- `How::Verus` in `hir-ty-contracts`: a fact now names a Lean proof (`How::Lean`).

`.clippy.toml` bans the removed names (`hir_ty_verus::proved`, `ProofKey`, `Clause`,
`verify_function`) with their replacements in `hir_ty_lean`.

What the tier did that nothing does now, each reported as "not proved", with why, never passed:

- **Proof with no proof written.** Z3 proved an `ensures` over integers and booleans (linear
  arithmetic, loop invariants with `decreases`) from the clause and the body alone. Lean
  checks the proof file a crate keeps in its `lean/` and searches for none: a clause without
  one is not proved.
- **Counterexamples.** A refuted clause came back with the parameters' values from Z3's model.
  Lean gives the goals left; contracts part 2's Hegel run is the source of counterexamples.
- **`invariant` and `decreases` as obligations.** The tier checked a loop's `invariant` and
  proved termination from `decreases`. The Lean translation states partial correctness for a
  function with a loop; `decreases`, `recommends` and a loop's `ensures` are refused by name.

The design that was not built (the kernel step and its key, `blaze check --verify`, ghost code,
the porting order of `rust_verify`, `rustc_public` for Kani) is in this file's history.
