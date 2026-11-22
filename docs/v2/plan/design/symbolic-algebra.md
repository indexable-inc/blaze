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

1. **Hash-consed Expression DAG (`ExprGraph<A: Allocator + Clone>`)**:
   - Every expression node (`Const`, `Var`, `Neg`, `Add`, `Sub`, `Mul`, `Div`, `Pow`, `Sin`, `Cos`,
     `Tan`, `Exp`, `Ln`, `Sqrt`, `Tanh`, `Abs`) is interned in O(1) amortized time via `blaze-hash`'s
     `Map<Op, ExprId, FixedState, A>` into a flat arena indexed by a typed `ExprId(u32)` newtype
     (never a tree, so repeated subexpressions are shared and symbolic derivatives stay O(|V|)).
   - Ergonomic tracing via `Sym<'g, A>` lets callers write ordinary formulas
     `(x * y).sin() / (g.lit(1.0) + x * x)` with standard Rust operators.
   - Deterministic evaluation uses `blaze-math` (`sin`, `cos`, `tan`, `exp`, `ln`, `sqrt`, `pow`),
     giving bit-identical results across platforms (rule 25).
2. **Forward & Reverse-Mode Symbolic Differentiation (`diff`, `grad`, `grad_reverse`, `jacobian`, `hessian`, `nth_diff`, `taylor`)**:
   - Memoized forward DAG differentiation (`diff`) and single-sweep O(|V|) reverse-mode symbolic
     adjoint differentiation (`grad_reverse`, `jacobian`, `hessian`) that compute all N partials
     simultaneously while sharing intermediate DAG nodes.
   - Verified by an independent forward-mode **Dual-Number Oracle (`Dual`, `eval_dual`)** in
     `blaze-sim` property tests with planted-bug controls (`WrongSinDeriv`, `WrongQuotientSign`,
     `WrongReverseAdjointAccum`; rule 43).
3. **Bounded E-Graph Equality Saturation (`EGraph<A>`, `simplify`)**:
   - Union-find e-classes with iterative path compression + O(1) SwissTable hashcons
     (`Map<ENode, EClassId, FixedState, A>`) + O(E) deterministic congruence closure + bounded
     saturation iterations (`SimplifyOpts`, rule 18) + cost-model DAG extraction.
   - Applies constant folding, additive/multiplicative identities, commutativity/associativity,
     distributivity/factoring (`a*b + a*c -> a*(b+c)`), power/log/exp cancellation
     (`ln(exp(x)) -> x`), and trig identities (`sin(x)^2 + cos(x)^2 -> 1`).
4. **Symbolic Equation Solver (`solve_for`)**:
   - Solves `lhs = rhs` symbolically for a target variable via structural unwrapping of invertible
     operations (`+`, `-`, `*`, `/`, `-u`, `exp`, `ln`, `sqrt`, `pow`) and exact derivative-based
     polynomial extraction (linear `-b / a` and quadratic `(-b ± sqrt(b^2 - 4ac)) / 2a`).
5. **Compile-Time `const fn` Expression Graph (`ConstGraph<const N: usize>`) & CSE SSA Emitter**:
   - Fixed-capacity stack/const expression DAG supporting `const fn` construction, symbolic
     differentiation, algebraic simplification, and `eval_const` inside `const` items today.
   - `emit_rust_fn` and multi-output `emit_rust_multi_fn` emit zero-allocation, CSE-shared SSA
     Rust functions (`pub fn f_and_grad(...) -> [f64; K]`) over `blaze_math`.
6. **Future Compiler Seam (`blaze::comptime::expr_of` / `splice!`)**:
   - When `blaze-comptime` exposes `expr_of(pure_fn) -> ExprGraph` and `splice!(graph, args...)`,
     this same library transforms ordinary `#[blaze::pure]` Rust functions at compile time and
     lowers the simplified derivative/solver DAG straight into MIR/LIR/SIMD/GPU with zero runtime
     overhead.

## Proof
- `blaze-sim` property test `blaze_sim_property_random_exprs_match_dual_oracle_and_egraph_preserves_value`
  across 64 random DAGs verifying forward `diff`, reverse `grad_reverse`, and e-graph `simplify`
  against `eval_dual`, with planted-bug controls (`WrongSinDeriv`, `WrongQuotientSign`,
  `WrongReverseAdjointAccum`) proved caught by the oracle.
- E-graph equivalence & cost-reduction property tests (`eval(orig) ≈ eval(simplified)` and
  `cost(simplified) <= cost(original)`).
- Symbolic solver substitution tests (`eval(lhs, x = root) ≈ eval(rhs, x = root)`).
- Compile-time `const` symbolic differentiation & simplification tests evaluated by `rustc`/`blaze`.
