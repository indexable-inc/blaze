---
status: plan
summary: lane tailcall: `become` (explicit_tail_calls) as a guaranteed tail call through the front end, the MIR evaluator, the stencils and LLVM; dev code is stencils, which make one, and wasm refuses it by name.
date: 2026-11-02
---
# Explicit tail calls (`become`)

Nightly Rust's `become f(args)` (`#![feature(explicit_tail_calls)]`, RFC 3407) promises that
the call replaces the caller's frame: a chain of any length runs in the stack of one, and the
caller's locals are dropped before the callee runs. Rule 4 makes it required: every program the
pinned nightly builds builds under blaze. Rule 3 makes the promise binding: a back end that
can't keep it refuses the call with an error naming it, never a call and a return in its
place.

## Layers

| Layer | State |
|---|---|
| Parse, HIR | `become` parsed and lowered (rust-analyzer's). |
| Checks | rustc's `check_tail_calls` ported as `crates/hir-ty-tailck` (query `tail_call_check`), rendered by `crates/blaze-diagnostics/src/lower/tail_call.rs`, in its order and with its messages: closures, operators, non-calls, `Fn`-trait and closure callees, intrinsics, non-fn callees, ABI mismatch, `rust-call`, signature mismatch (not for an `extern "tail"` callee), `#[track_caller]` caller, C-variadic caller or callee, unsized arguments. Borrows of locals live across the call are E0597 from borrowck, as in rustc, since the locals' `StorageDead` comes before the `TailCall`. A body with a refused `become` is never lowered to MIR or borrow-checked, as rustc taints it. |
| MIR | `TerminatorKind::TailCall` (`crates/hir-ty-mir/src/mir/lower/tail_call.rs`): locals dropped before the call in rustc's `break_for_tail_call` order; no unwind edge, no successors. |
| MIR evaluator | `exec_tail_call`: the frame is replaced, so const eval of a deep chain stays flat (`crates/hir-ty-mir/tests/mir/tail_call.rs`). |
| IR builder | `FunctionCx::tail_call` in `crates/hir-ty-codegen/mod.rs`: a `return_call` / `return_call_indirect` under the target's default convention, described below. `v2/refs.rs` lists the one callee, never built in. |
| Stencils | `return_call` between functions of SystemV, AppleAarch64 and WindowsFastcall on x86-64 and AArch64 (`crates/blaze-stencil/src/tests/func/tail.rs`). |
| LLVM | `musttail` call then `ret` (`crates/hir-ty-codegen/llvm/lower.rs`, `Lowering::tail_call`), through the caller's own function type, ABI attributes copied from the caller. |
| Plan | Nothing special (2026-11-02): `Policy::choose` (`crates/blaze-codegen/src/plan.rs`) gives native code at 0 and 1 to stencils, a live build's first tier too, so `become` builds as any other code. The LLVM-with-no-passes stopgap, `Described::tail_calls` and `PlanError::TailCallsNeedLlvm` are gone. |
| Cranelift native, wasm | Refused by name in `emit_with` (`refuse_tail_calls`): Cranelift tail-calls only under its `tail` convention, which no Rust function has; the wasm lowering has no `return_call` yet. The plan never sends native code there; the check stays for a tiered run's Cranelift tier (`blaze_stencil::tier`, to be removed under rule 8), v1's compile and wasm. |
| Fold (#1292) | A function with a `return_call` is not folded: the thunk's plain call would leave a frame under every hop. |

## The IR contract

What the IR builder hands a back end, and what each back end relies on:

- The callee's convention is the caller's, and so are its return values (the front end has
  checked the signatures; the builder checks the Cranelift signatures again for a direct
  callee and refuses a difference).
- An indirect result is written where the caller was asked for its own: the `StructReturn`
  argument is the caller's own entry parameter.
- An argument passed by reference (`PassMode::Indirect`) is copied first into a temporary,
  then into the memory the caller was handed for its parameter at the same place, and that
  pointer is passed, as rustc's `tail_call_temporaries` does. The two steps let
  `become f(b, a)` swap two such parameters; every other argument is loaded before the
  second step writes any of the caller's own memory.
- Nothing else of the caller's frame is passed: borrowck refuses a borrow of a local, and the
  builder never hands over a slot (`given_away`) for a tail call.
- Never a `try_call`: no landing pad of the caller outlives the call.
- A `#[track_caller]` callee is reached through its `ReifyShim`, as rustc does (its lint
  `tail_call_track_caller` says the callee is told no caller).

## Gaps (typed errors today)

- **A tiered run's Cranelift tier** (`blaze_stencil::tier::Tier::Cranelift`) refuses a hot
  function with `become` when it tiers up; the tier goes under rule 8.
- **`become` through a vtable** (`become obj.method(..)` on `dyn Trait`): the callee takes a
  thin receiver where the caller has a fat pointer, so the parameter lists differ. The stencils
  accept it when the callee's stack arguments fit the caller's area; LLVM's verifier refuses
  a `musttail` with another prototype (rustc emits one unverified), so release refuses it.
  Fix: a shim of the caller's prototype, or the receiver widened.
- **By-value stack arguments**: `PassMode::OnStack` and a spilled `SpilledPair` (an `i128`
  in the last register on x86-64 System V, C ABIs). The builder refuses them; the stencils can
  forward a caller's own `StructArgument` in place, so writing the new value into it first is
  the next step.
- **Windows x64 16-byte values passed by pointer**: refused by the stencils and LLVM (the copy
  would go in the frame the call leaves).
- **`extern "tail"` callees with another signature**: rustc allows them (tailcc); blaze has no
  `tail` convention for Rust functions, so the signature check refuses them.
- **wasm** (proc macros): needs Wasmtime's tail-call proposal and a `return_call` in the
  lowering.
- **Front end**: the `tail_call_track_caller` warning comes at check time, not codegen, so it
  also fires for generic callers never instantiated, and misses an impl method that is
  `#[track_caller]` where the trait's declaration isn't and the call resolves only after
  monomorphization (rustc's `default-trait-method.rs`). `blaze-item-hash` has no read rule for
  `tail_call_check` yet, so a body with a `become` is computed and not stored.
- **Tier-up**: a stencil function with a native tail call can't go to Cranelift; it tiers up
  only to LLVM.

## Tests

- `crates/hir-ty-codegen/v2/lowered/tests.rs`,
  `become_is_a_return_call_in_the_callers_own_memory`: the stencil IR of three `become`s is one
  `return_call` of the caller's signature; `swap` hands on exactly its own incoming memory.
- `crates/blaze-stencil/src/tests/func/tail.rs`: native-convention tail calls run 2^20 hops at
  stack depth 0, swap stack arguments, chain `StructReturn`, forward a `StructArgument`.
- `crates/hir-ty-codegen/llvm.rs` tests: `musttail` then `ret`, sret passed on, indirect, a
  refusal; the self call survives O3.
- `harness/wholeprogram.py '{"cases": ["tail_calls"]}'`: an interpreter of ten million steps
  on a 64 KiB stack, a `countdown` returning through memory, rustc's `drop-order.rs`,
  `indirect.rs` and `callee_is_track_caller.rs`, output as the pinned nightly prints it, in
  dev and with `"profile": "release"` alike.
- `crates/blaze-codegen/src/plan.rs`, `a_tail_call_at_a_cranelift_level_is_llvms_with_no_passes`.
- `crates/blaze-diagnostics/src/tests/lints/mir/tail_calls.rs`: the refusals with rustc's
  messages (`become_needs_matching_signatures_and_abis`, `what_become_refuses`, ...), E0597 for
  a borrow of a local, and the `#[track_caller]` warning.
