---
status: plan
summary: Formal verification in blaze by forking Verus. Verus's syntax is native blaze grammar, gated per crate by `#![feature(blaze_verify)]` and lowered in hir-def. crates/hir-ty-verus lowers blaze's HIR into Verus's VIR (replacing rust_verify, rule by rule), and vendored vir and air prove it with a pinned, seeded, resource-limited Z3. Each function's proof is a kernel step keyed by its VIR, and `blaze check --verify` runs it. Stage 2 sizes rustc_public on blaze's MIR, so Kani can run.
date: 2026-11-02
---
# Verus in blaze: proofs without rustc

Lane `verus`. The handoff is `/tmp/blaze-lanes/v2-verus.md`. Andrew decided the spec form on
2026-11-02: there is one, Verus's own syntax, as blaze grammar, with no `verus!` wrapper.

## What landed (first slice)

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
- **Vendored Verus** (`vendor/verus`, SOURCE.md): `air`, `vir`, `vir_macros`, `builtin` and
  `vstd`, at commit `3881d449`. vstd's top-level `verus!` blocks are stripped by
  `tools/v2/verus_strip.py`.
- **`crates/hir-ty-verus`.** It lowers one function from HIR to VIR (`lower.rs`), then runs
  VIR's checks and passes, the AIR encoding and Z3 (`driver.rs`, `commands.rs`).
  - The lowering covers requires and ensures over integers and booleans, `let`, assignment,
    `if`, `loop` and `while` with invariants and `decreases`, and `break`, `continue` and
    `return`.
  - Exec arithmetic is overflow-checked; spec arithmetic is over `int`.
  - Everything else is a typed `Unsupported` naming the construct and its source text, never
    a guess.

## The interface (fixed with lane contracts)

```rust
pub fn proved(db: &Db, func: FunctionId, args: StoredGenericArgs, clause: Clause) -> Option<ProofKey>;
pub enum Clause { Requires, Ensures }
pub struct ProofKey(pub ContentId);
pub fn verify_function(db: &Db, func: FunctionId, config: &Config) -> Verdict;
```

- **Answers.** Only `Outcome::Proved` answers `Some`. `Unknown` is never a proof.
- **Outcomes.** `Verdict.outcome` is one of five:
  - `Proved`;
  - `Refuted(Vec<Refutation>)`, where each refutation names the clause (`Obligation::Clause`)
    or the code that failed, plus a `Counterexample` of the parameters' values from Z3's model;
  - `Unknown`, either `Rlimit` (out of resources) or `Incomplete` (Z3's own "incomplete");
  - `Unsupported`;
  - `Error`, naming the stage.
- **Counterexamples.** Lane contracts' part 2 seeds its Hegel test from a refutation's
  counterexample.
- **`Requires`.** It answers `None` until call sites are lowered (see the porting order below).
- **`#[core::contracts::*]`.** It is not a blaze spec form. It stays only because rule 4 needs
  every nightly program to build. Lane contracts adds a `blaze check --fix` lint that rewrites
  it to the native clauses.

## Solver: deterministic, and "unknown" stays unknown

- **Pinned Z3.** Z3 4.16.0 comes from the flake as `BLAZE_Z3` (`flake.nix`).
  - air refuses any other version: `set_expected_solver_version`.
  - blaze names the executable itself (`Context::start_solver`). Nothing is found on PATH.
  - A missing `BLAZE_Z3` is an error, never a skip.
- **Fixed seeds.** `smt.random_seed` and `sat.random_seed` are both 0.
- **Resource limit.** Z3's `rlimit` replaces a timeout: `Config::rlimit`, by default
  `10 * 3_000_000`, which is Verus's per-second conversion. So a proof that runs out runs out
  on every machine alike.
- **Unknown is a typed outcome.**
  - A `Canceled` query is `Unknown{Rlimit}`.
  - Z3's "incomplete" unknown is `Unknown{Incomplete}`. Upstream air reported it as `Invalid`
    with a model; the vendored air reports it apart (`last_unknown_incomplete`), so no
    refutation is made from it.
- **Refuted queries.** blaze asks for more failing assertions of a refuted query, at most 64
  more (rule 15). Past 8, it only checks earlier ones, as Verus's `--multiple-errors` does.
- **Panics.** A panic in vendored code fails that function's verdict with the panic's message
  and nothing else (rule 17).

## The kernel step and its key

Each function's proof is one step: `verify_function(func)`.

- **The key** (`ProofKey`) is a blake3 hash over:
  - the vendored Verus commit;
  - the Z3 version;
  - the rlimit and the seed;
  - the printed VIR crate the function lowers to (`vir::printer::write_krate`, with no spans).
- **What the key covers.** That crate holds the function's clauses, body, types and arch. Once
  calls are lowered, it also holds each callee's specification, as a body-less function. A
  callee's body is not an input (modular verification), so editing a callee's body without
  touching its spec reuses the caller's proof.
- **Rule 18.** The key covers every input the verdict reads: the AIR encoding reads only that
  crate and the solver settings. Two verdicts under one key is a refusal, never an overwrite.
- **Persisting.** The step becomes a `blaze_query` query on `(FunctionId, StoredGenericArgs)`
  whose value persists in the store under `ProofKey`. A warm `blaze check --verify` reruns only
  the proofs whose key changed. A round-trip test restores the verdicts and compares them with
  a fresh run (rule 23). That test is not written yet.
- **What callers use.** A caller optimised on a proved clause puts `ProofKey` in its own code
  identity (lane contracts, part 3).

## `blaze check --verify`

Verification is a check: it produces no code and only reports. So it goes on `check`, the
command whose diagnostics agents and editors already read, rather than a new `blaze verify`
command with its own output.

- **Why a flag.** Proofs cost solver time, which a plain `check` should not spend on every save.
- **Feature versus flag.** `#![feature(blaze_verify)]` says which crates have specs.
  `--verify` says to prove them now.
- **Builds.** `blaze build` without the flag erases the clauses and builds as before.
- **Output.** A refutation is a diagnostic at the clause, carrying the counterexample's values.
  An unknown is a warning naming the function and how much of the rlimit it used. It is never
  shown as success.
- **Status.** The CLI wiring is not in this slice.

## Ghost code

Verus erases ghost code, then has rustc borrow-check a generated "lifetime" program of the
`tracked` parts (`rust_verify/src/erase.rs` and the lifetime generator). blaze does the same
with its own borrow checker:

- **Today.** Clause groups are ghost, and MIR building drops them.
- **Modes.** `spec`/`proof` functions and `ghost`/`tracked` bindings come with the grammar (see
  the order below). VIR's `modes::check_function` already returns `ErasureModes` for each
  expression.
- **The ghost body.** `hir-ty-mir` builds a second MIR body per function with the spec code
  removed and the `tracked` code kept. `hir-ty-borrowck` checks it, standing in for Verus's
  lifetime program, and the body is then dropped.
- **The real body.** The function's real MIR has all ghost code erased. So a program that uses
  no Verus syntax compiles exactly as before (rule 4).

## Ported and left in `rust_verify`, in order

| upstream file | lines | status |
|---|---|---|
| `query.rs` (checked/merged crate pipeline) | 717 | ported for one crate with no imports (`driver.rs`) |
| `verifier.rs` (`verify_bucket`, `check_result_validity`) | 3312 | the core is ported; expand-errors, recommends retries, profiling, Singular and spinoff provers are left |
| `commands.rs` (`OpGenerator`) | 556 | ported, trimmed (`commands.rs`) |
| `rust_to_vir_func.rs` | 3969 | header, params and named return only: no generics, `self`, modes, attributes or trait methods |
| `rust_to_vir_expr.rs` | 5454 | int/bool literals, locals, arithmetic, comparison, logic, `if`, loops, `let`, assignment, `break`/`continue`/`return` |
| `rust_to_vir_base.rs` (types, paths) | 2825 | integer and bool types only |
| `spans.rs`, `context.rs`, `buckets.rs`, `user_filter.rs` | | replaced by `Origin` spans and per-function steps |
| `fn_call_to_vir.rs`, `verus_items.rs`, `rust_intrinsics_to_vir.rs` | 4474 | left |
| `rust_to_vir_adts.rs`, `rust_to_vir_ctor.rs` | 1013 | left |
| `rust_to_vir_trait.rs`, `rust_to_vir_impl.rs`, `trait_check*.rs`, `trait_conflicts.rs` | 3158 | left |
| `external.rs`, `attributes.rs`, `reveal_hide.rs`, `hir_hide_reveal_rewrite.rs` | 3035 | left |
| `erase.rs` and the lifetime program | 578+ | replaced by the ghost-body borrowck above |
| `singular.rs`, `expand_errors_driver.rs`, `boundary_suggestions.rs` | 1925 | left |
| `main.rs`, `driver.rs`, `config.rs`, `cargo_verus.rs`, `test_observer.rs` | | not ported: blaze's CLI and tests replace them |

The order, each step landing with its own end-to-end test:

1. **Calls.** `fn_call_to_vir`: calls to non-generic functions, with the callee's specs pulled
   into the caller's VIR crate. Call sites check `requires`, so `proved(.., Requires)` answers.
2. **Spec and proof functions.** The grammar gets `spec fn`, `proof fn`, `open`/`closed`,
   `ghost`/`tracked`, `forall`/`exists`, `==>`, `&&&`/`|||`, `#[trigger]`, `old`, `@`,
   `assert`/`assume` and `by`. Then `verus_items` maps vstd's and builtin's paths to VIR's
   built-ins, and vstd parses under blaze.
3. **ADTs, constructors and `match`.**
4. **References and `&mut`.** `old`/`final`, and the ghost-body borrowck.
5. **Generics, traits and impls.** `proved` then answers for instances, not only non-generic
   functions.
6. **External specifications** (`assume_specification`), and vstd's `std_specs` for the real
   std.
7. **The rest.** Closures and spec function types, broadcast and reveal/hide; `by (compute)`
   (the interpreter's wall-clock limit becomes fuel); `integer_ring` (Singular, pinned in the
   flake); `bit_vector`; `nonlinear_arith`.
8. **Many modules and crates.** Each crate's VIR persists in the store, as upstream's `.vir`
   files do; then expand-errors, and profiles as spans (rule 14).

## Stage 2 (plan only): `rustc_public` on blaze's MIR, so Kani runs

Kani's compiler, at commit `f5a9a9e6`, is 43k lines in 82 files.

- **Its upstream surface.** It reads rustc through `rustc_public`, plus internals:
  - `rustc_public::{mir, ty, abi, target, visitor, crate_def}` and the queries `all_local_items`,
    `entry_fn`, `find_crates`, `local_crate` and `all_trait_decls`;
  - `rustc_public::rustc_internal`: 162 `internal(..)` and 60 `stable(..)` calls in 37 files.
    These are the places where it leaves the stable API.
- **The internals it uses**, by count of paths:
  - `rustc_middle::mir` (154) and `rustc_middle::ty` (131). Most are in
    `kani_middle/transform/internal_mir.rs`, which turns stable MIR back into rustc's to install
    transformed bodies, and in `attributes.rs`, `codegen_units.rs`, `resolve.rs` and the
    goto-c `typ.rs`;
  - `rustc_hir` (def ids, coroutine kinds, `Safety`);
  - `rustc_span`;
  - `rustc_session::config`;
  - `rustc_codegen_ssa::{back, traits}` (the `CodegenBackend` it implements);
  - `rustc_metadata::EncodedMetadata`;
  - `rustc_interface` and `rustc_driver` (the entry point and query providers);
  - `rustc_abi`, `rustc_target::{spec, callconv}`, `rustc_index`, `rustc_data_structures`,
    `rustc_attr_ir::lang_items`, `rustc_trait_selection`, `rustc_infer`, `rustc_mir_dataflow`,
    `rustc_errors`, `rustc_parse`, and the AST and HIR pretty-printers.
- **What blaze must provide**, as a crate `rustc_public` over blaze's MIR and type IR, named so
  that Kani builds unchanged:
  - **The `rustc_public` API.** Its `CompilerInterface`, about 150 methods; count them at the
    pinned nightly before starting. These map onto hir-ty's MIR, layout, const eval and
    instances.
  - **A shim for each internal above.** Most are small: ids, spans, symbols, session config.
  - **Two large ones.** First, the internal-MIR round trip (`internal_mir.rs`), which needs a
    `rustc_middle::mir`-shaped view of blaze's MIR. Second, Kani's `CodegenBackend`, which blaze
    replaces by running Kani's goto-c codegen as a blaze backend fed by blaze's mono items.
- **Size.** The `rustc_public` surface, plus about 25 internal paths shimmed, plus the MIR round
  trip, plus a backend hook. Of these, `internal_mir.rs` and the 162 `internal` calls are the
  risk; the stable surface is the easy part.
- **Rules.** It needs no rustc at run time (rule 1). Kani's own test suite under blaze is the
  measure (rule 25).

## Prior art

Every Rust verifier found drives rustc, through `rustc_driver` callbacks or `rustc_public`:

| verifier | how it uses rustc |
|---|---|
| Verus | forks of `rustc_hir_typeck` and `rustc_mir_build` |
| Creusot | rustc's MIR, then Why3 |
| Prusti | MIR and Polonius facts, then Viper |
| Kani | `rustc_public` and internals, then CBMC |
| Flux | rustc's HIR and MIR |
| Aeneas/Charon | rustc's MIR, then LLBC and Lean |
| hax | rustc's THIR |

verus-analyzer is the one tool on rust-analyzer's front end (blaze's front end derives from it),
and it is IDE-only: syntax and navigation, no proofs. Nothing found verifies Rust from a front end
that is not rustc. This lane takes Verus's rustc-free middle (VIR and AIR, by design free of
rustc: "do not add any dependencies on rustc" in their manifests) and replaces only the rustc
end.

## Open points

- **Name collision (resolved 2026-11-02).** The store check crate (`BLAZE_VERIFY_STORE`) was
  `blaze-verify`, the same name as `#![feature(blaze_verify)]`; it is now `blaze-store-check`.
- **Spec equality between a machine integer and `int`.** `r == n + 1` with `r: u32` relies on
  VIR's integer coercion in spec mode. Typing it the way Verus does (`int` widening in specs)
  belongs in hir-ty, as rule 26 says.
- **Determinism.** VIR uses std `HashMap` in places. The proof key hashes printed VIR, which
  iterates vectors only, but the AIR command order still has to be checked byte-equal across
  two runs (rule 22). That test is not written yet.
- **Async and gen functions.** Their clauses land in the coroutine body. Such functions are
  refused until that is handled.
