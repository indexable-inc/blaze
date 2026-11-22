---
status: plan
summary: "blaze-algebra (`bstd::algebra`): zero-dependency, `#![no_std]`, `#![blaze::sans_io]` symbolic algebra, memoized DAG symbolic differentiation, bounded e-graph equality saturation, symbolic equation solving, and `const fn` comptime expression graphs — plus the two compiler seams (`expr_of` / `splice!`) connecting pure Rust functions to comptime symbolic transforms."
date: 2026-11-22
---
# Symbolic Algebra & Differentiation (`bstd::algebra`)

## Design (Rule 32 `library-over-language` + Rule 55 `own-the-language`)

Symbolic differentiation, algebraic simplification, equation solving, Taylor expansion, and CSE
code generation belong in a **zero-dependency `#![no_std]` library** (`crates/blaze-algebra`,
re-exported as `bstd::algebra`), backed by two small compiler seams when lifting arbitrary
`#[blaze::pure]` Rust functions (`docs/future/autodiff.md`):

1. **Hash-consed Expression DAG (`ExprGraph<A: Allocator + Copy>`)**:
   - Every expression node (`Const`, `Var`, `Neg`, `Add`, `Sub`, `Mul`, `Div`, `Pow`, `Sin`, `Cos`,
     `Tan`, `Exp`, `Ln`, `Sqrt`, `Tanh`, `Abs`, piecewise differentiable `Select` powering `relu`,
     `max`, `min`, and `clamp`, and fused multiply-add `Fma` via `blaze_math::mul_add`) is interned
     in O(1) amortized time via `blaze-hash`'s `Map<Op, ExprId, FixedState, A>` into a flat arena
     indexed by a typed `ExprId(u32)` newtype (never a tree, so repeated subexpressions are shared
     and symbolic derivatives stay O(|V|)).
   - Ergonomic tracing via `Sym<'g, A>` lets callers write ordinary formulas
     `(x * y).relu() + x.max(y) - (x * y).sin()` with standard Rust operators.
   - Deterministic evaluation uses `blaze-math` (`sin`, `cos`, `tan`, `exp`, `ln`, `sqrt`, `pow`,
     `mul_add`), giving bit-identical results across platforms (rule 25).
2. **Forward, Reverse-Mode & Second-Order O(|V|) Differentiation (`diff`, `grad`, `grad_reverse`, `jvp`, `hvp`, `active_vars`, `jacobian`, `hessian`, `nth_diff`, `taylor`)**:
   - Memoized forward DAG differentiation (`diff`), single-sweep forward directional derivative
     (`jvp`), single-sweep O(|V|) reverse-mode symbolic adjoint differentiation (`grad_reverse`,
     `jacobian`, `hessian`), O(|V|) Hessian-Vector Products (`hvp = ∇(jvp(f, v))`), and O(|V|)
     reachability sparsity detection (`active_vars`), all supporting piecewise `Select` and `Fma`.
   - Verified by an independent forward-mode **Dual-Number Oracle (`Dual`, `eval_dual`)** in
     `blaze-sim` property tests with planted-bug controls (`WrongSinDeriv`, `WrongQuotientSign`,
     `WrongReverseAdjointAccum`; rule 43).
3. **Bounded E-Graph Equality Saturation (`EGraph<A>`, `simplify`, `simplify_with`, `simplify_with_rules`)**:
   - Union-find e-classes with iterative path compression + O(1) SwissTable hashcons
     (`Map<ENode, EClassId, FixedState, A>`) + O(E) deterministic congruence closure + bounded
     saturation iterations (`SimplifyOpts`, rule 18) + target cost-model DAG extraction
     (`CostModel::{Balanced, CpuFma, GpuTranscendental}`, fusing `a * b + c -> fma(a, b, c)`).
   - **Floating-point soundness policy (`RewriteMode::{IeeeExact, FastMath}`)**: `IeeeExact`
     applies only IEEE-754-safe identities (never `x * 0 -> 0`, `x / x -> 1`, reassociation, or
     FMA fusion); `FastMath` unlocks real-algebraic identities (`sin(x)^2 + cos(x)^2 -> 1`,
     `ln(exp(x)) -> x`, factoring, and FMA fusion).
   - **Outward-rounded Interval Range Analysis & Dead-Branch Pruning (`interval.rs`)**: reuses
     `blaze_math::Interval` (`one-concept-one-home`, rule 30) to propagate conservative `[lo, hi]`
     bounds across `ExprGraph` and soundly eliminate provably dead `Op::Select` branches, signed
     `abs(x)`, and non-zero finite `x / x -> 1`.
   - **Declarative User Rewrite Rules (`PatNode`, `RewriteRule<'r>`, `simplify_with_rules`)**:
     zero-heap, `const`-constructible postorder pattern slices with bounded e-matching so callers
     can supply domain identities (e.g. `exp(?0) * exp(?1) => exp(?0 + ?1)`).
4. **Symbolic Equation, Linear System & Exact Newton-Raphson Solvers (`solve_for`, `solve_linear_system`, `newton_step`, `eval_newton`)**:
   - `solve_for`: solves `lhs = rhs` symbolically via structural unwrapping of invertible operations
     (`+`, `-`, `*`, `/`, `-u`, `exp`, `ln`, `sqrt`, `pow`) and exact polynomial extraction
     (linear `-b / a` and quadratic `(-b ± sqrt(b^2 - 4ac)) / 2a`).
   - `solve_linear_system`: exact symbolic Gaussian elimination with back-substitution for bounded
     `N x N` (`N <= 8`) parametric linear systems `A * x = b`.
   - `newton_step` & `eval_newton`: builds the exact simplified symbolic Newton step
     `x - f(x) / f'(x)` and evaluates bounded quadratically-convergent root-finding (`NewtonOpts`,
     `NewtonResult`).
5. **Exact Rational & Canonical Sparse Multivariate Polynomial Engine (`poly.rs`)**:
   - Exact reduced `Rat` (`i128/u128`, overflow-checked) and canonical sparse multivariate
     `Poly<A: Allocator + Copy>` implementing `blaze_own::CloneIn` (zero implicit `Clone` impls,
     zero `.clone()` calls, zero redundant allocator field on `Monomial`).
   - Exact polynomial differentiation (`Poly::diff`), univariate polynomial long division
     (`div_rem_univariate`), monic Euclidean polynomial GCD (`gcd_univariate`), and bi-directional
     `expr_to_poly` / `poly_to_expr` / `simplify_poly_expr` (cancelling both multivariate
     identities like `(x + y)^3 - (x^3 + 3x^2y + 3xy^2 + y^3) == 0` and rational functions like
     `(x^3 - 6x^2 + 11x - 6) / (x - 2) == x^2 - 4x + 3`, wired directly into `solve_for`).
6. **Compile-Time `const fn` Expression Graph (`ConstGraph<const N: usize>`) & CSE SSA Emitter**:
   - Fixed-capacity stack/const expression DAG supporting `const fn` construction (including
     `select`, `relu`, `max_expr`, `min_expr`, `fma`), symbolic differentiation, algebraic
     simplification, and `eval_const` inside `const` items today.
   - `emit_rust_fn` and multi-output `emit_rust_multi_fn` emit zero-allocation, CSE-shared SSA
     Rust functions (`pub fn f_and_grad(...) -> [f64; K]`) over `blaze_math`.
7. **Proof-Carrying MIR Compiler Seam (`crates/blaze-algebra-body`)**:
   - `lift_body`: lifts verified pure acyclic `blaze_body::Verified<'_>` — including comparison
     `StmtKind::Switch` diamonds and nested `if/else` returns — into `ExprGraph<A>` (`Op::Select`),
     refusing cycles and impure calls with typed `BodyAlgebraError` naming `Block` and `Stmt`.
   - `splice_body` & `splice_multi_body`: CSE topological walk emitting a fresh
     `blaze_body::Body<A>` — including math `Call` continuation blocks (including 3-arg
     `MathOp::Fma`), `BinOp::Gt` + `StmtKind::Switch` diamond CFGs for `Op::Select`, and
     `Value::Aggregate { kind: Array, .. }` for multi-output `[f64; K]` bodies — checked via
     `blaze_body::verify`.
   - `diff_body`, `grad_bodies`, `value_and_grad_body` (fused `[f, ∂f/∂x_0, ...]`), `hvp_body`
     (fused 2N-arg O(|V|) Hessian-Vector Product `H(f)(x) · v`), and `simplify_body_with`
     (supporting `RewriteMode::IeeeExact`, `CostModel::CpuFma`, and interval dead-branch pruning)
     run end-to-end `Verified<Body>` → verified `Body` compiler transforms today.

## Proof
- `blaze-algebra` (`14 passed, 0 failed`, 0 `.clone()` calls): `blaze-sim` property tests over
  random DAGs (`diff`, `grad_reverse`, e-graph `simplify` vs `eval_dual`) and random exact
  polynomials (`(p*r)/r == (p,0)`, Euclidean GCD divisibility, `Poly::diff == Dual tangent`),
  with 5 planted-bug controls (`WrongSinDeriv`, `WrongQuotientSign`, `WrongReverseAdjointAccum`,
  `WrongGcdRemainder`, `WrongPolyDiffCoeff`) proved caught by the oracle, plus `Op::Select`
  (`relu`/`max`/`min`/`clamp` + `const fn`), `Op::Fma` + `CostModel::CpuFma`,
  `RewriteMode::IeeeExact` vs `FastMath`, `blaze_math::Interval` dead-branch pruning, `jvp`/`hvp`/
  `active_vars` sparsity vs full Hessian, custom `PatNode`/`RewriteRule`, symbolic linear system
  solving, and exact Newton-Raphson.
- `blaze-algebra-body` (`8 passed, 0 failed`, 0 `.clone()` calls): end-to-end MIR → `diff_body` /
  `grad_bodies` / `value_and_grad_body` / `hvp_body` / `simplify_body_with` → `blaze_body::verify`
  → `eval_pure_body` / `eval_pure_multi_body` verified against `eval_dual` and `hessian` across
  rational bodies, multi-block transcendental `Call` bodies, round-trip `Switch` diamond lifting +
  interval branch pruning (7+ blocks collapsed to 1), fused value-and-gradient & HVP CSE MIR
  bodies, FMA MIR fusion, exact polynomial collapsing to 2 MIR rows, cyclic refusal, and 32
  `blaze-sim` random MIR bodies with planted-bug control `SpliceOriginalInsteadOfDeriv`.
- E-graph equivalence & cost-reduction property tests (`eval(orig) ≈ eval(simplified)` and
  `cost(simplified) <= cost(original)`).
- Symbolic solver substitution tests (`eval(lhs, x = root) ≈ eval(rhs, x = root)`).
- Compile-time `const` symbolic differentiation & simplification tests evaluated by `rustc`/`blaze`.
