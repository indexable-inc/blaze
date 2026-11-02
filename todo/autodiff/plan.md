---
status: plan
summary: Differentiation in blaze on CPU and GPU, from automatic differentiation on MIR, to symbolic derivatives of pure functions, to e-graph simplification shared by both.
date: 2026-11-02
---
# Automatic and symbolic differentiation

blaze owns MIR, so it can differentiate Rust functions itself, the same way for the CPU and for
the GPU kernels in [../gpu/plan.md](../gpu/plan.md). There are three layers, each usable on its
own:

1. **Automatic differentiation on MIR.** Handles any function: loops, branches, memory.
2. **Symbolic derivatives** of functions blaze can lift to an expression. Closed forms you can
   read, and exact Hessians.
3. **E-graph simplification**, shared by both, which shrinks the code either one produces.

Nothing here is built. Like every blaze extension, it's opt-in (`superset-of-rust`): a crate that
doesn't use `#[blaze::differentiable]` builds exactly as before.

## Why in blaze

`std::autodiff` runs Enzyme on LLVM IR. That's late in the pipeline: rustc's types are gone, and
GPU code that goes MIR to SPIR-V never passes through LLVM, so it gets nothing. JAX, Zygote and
Slang differentiate their own IR before lowering, and that's the level blaze has. Slang's
`[Differentiable]` and `bwd_diff` are the ergonomics to match. It's the one Slang feature with no
Rust equivalent on a GPU.

## Layer 1: automatic differentiation on MIR

```rust
#[blaze::differentiable]
fn loss(w: &[f32], x: &[f32], y: f32) -> f32 {
    let mut s = 0.0;
    for i in 0..w.len() { s += w[i] * x[i]; }
    (s.tanh() - y).powi(2)
}

let (l, dw) = blaze::grad!(loss, wrt = w)(&w, &x, y);  // reverse mode
let dl = blaze::jvp!(loss, wrt = w)(&w, &x, y, &dir);  // forward mode
```

- Forward mode (JVP) and reverse mode (VJP) both rewrite monomorphized MIR, after borrow checking.
  The result is a new MIR body that goes down the normal codegen path: native, SPIR-V or PTX.
- MIR shares common subexpressions as locals, so derivatives don't blow up the way naive
  expression differentiation does.
- Reverse mode needs a tape for the values the backward pass reads. The tape uses the frame
  allocator (`bump-allocate`). On a GPU it lives in thread-local or workgroup memory, sized at
  compile time or refused with an error naming the loop that makes it unbounded.
- Derivative rules for std math (`sin`, `exp`, `powi`, `sqrt`) live in one table next to the
  intrinsics. A user writes a custom rule with `#[blaze::derivative(of = f)]` (numerically stable
  `log1p`, a solver's implicit derivative).
- A function with no rule (an FFI call, a non-`differentiable` dependency, a value through a raw
  pointer) is a typed error naming the function and the call. It's never silently treated as
  constant (`no-fallback`). `blaze::stop_gradient(x)` marks a constant on purpose.

## Layer 2: symbolic derivatives

A function blaze can lift to a pure expression gets a derivative in closed form:

```rust
#[blaze::pure]
#[blaze::differentiable(symbolic)]
fn f(x: f64, y: f64) -> f64 { (x * y).sin() / (1.0 + x * x) }

// blaze explain d/dx f
//   (y * cos(x * y) * (1 + x²) - 2x * sin(x * y)) / (1 + x²)²
```

- **What lifts.** A function `#[blaze::pure]` proves pure (`crates/hir-ty-pure`), over scalars,
  small fixed arrays and SIMD vectors. Its loops must have constant trip counts (unrolled) and its
  branches must turn into `select`. Everything else stays in layer 1.
- **Lifting.** Monomorphized MIR becomes an expression graph with shared nodes. The derivative is
  taken there, rule by rule, then simplified (layer 3) and lowered back to MIR.
- **What you get beyond layer 1:** simplified Hessians and Jacobians, which matter for Newton
  steps, optimizers and physics; derivatives you can read, through `blaze explain` and printable
  as Rust source; and exact derivatives where the derivative of the code isn't the derivative of
  the math. For example, `x.abs()` at 0 gets a typed choice, not an accident of how `abs` was
  written.
- **When lifting fails**, `differentiable(symbolic)` reports why (the loop, branch or call that
  didn't lift) and what layer 1 would do. The user removes `symbolic`. It never switches modes on
  its own (`no-fallback`).
- **Floating point.** Simplification can change rounding. Only rewrites that are exact in IEEE
  754 apply by default. Algebraic rewrites (`x*y/y → x`, reassociation) need
  `#[blaze::differentiable(symbolic, fast_math)]`, and `blaze explain` lists which ones ran.

## Layer 3: e-graph simplification

Equality saturation (egg-style e-graphs) on the expression graph, for layer 2's derivatives and
for layer 1's output where a region lifts.

- Rules include `sin² + cos² → 1`, `exp(log x) → x` (for `x > 0`, from a range fact), `x * 0 → 0`
  (except NaN and infinity, unless `fast_math`), factoring and common-factor extraction.
- Extraction picks the cheapest term under a cost model per target (FMA-aware on CPUs, and
  counting transcendental calls on GPUs).
- Every run has a bound: node count and iteration fuel (`bounded-progress`). Hitting the bound ends
  in a typed outcome that says how far it got. The best term found so far is valid and gets used,
  and `blaze explain` reports that the bound stopped it.
- The rule set is data with one owner. Each rule carries the IEEE conditions it holds under, and a
  Hegel property test checks each rule numerically over its domain.

## Where it's a moonshot

- General programs. Simplification has no complete algorithm, data-dependent loops and memory
  don't lift, and some derivatives can't be written in closed form. Layer 1 covers these. Symbolic
  differentiation is for the pure numeric core, which in practice holds most of the math.
- Higher-order reverse mode (Hessian-vector products through a tape) gets expensive. Forward over
  reverse is the usual answer, and the symbolic layer makes it cheaper where it applies.
- GPU tapes are limited by thread-local and workgroup memory. Checkpointing (recompute instead of
  store) has to be a planned choice, reported by `blaze explain`.

## Phases

1. Forward mode (JVP) on MIR for scalar functions, CPU only. `blaze::jvp!`.
2. Reverse mode with a frame-allocated tape, loops and slices. `blaze::grad!`.
3. The same transforms on GPU kernels, with tapes in thread or workgroup memory
   ([../gpu/plan.md](../gpu/plan.md) phase 6).
4. Lifting pure functions to expressions, symbolic derivatives, and `blaze explain d/dx`.
5. E-graph simplification with IEEE-exact rules, then `fast_math` rules, with per-target cost
   models.
6. Simplified Hessians and Jacobians, and simplification of layer 1 regions that lift.

## Prior art

- Enzyme (`std::autodiff`): AD on LLVM IR. It's fast, but it runs after types are gone and never
  sees SPIR-V.
- JAX: `jvp`, `vjp`, `grad` as transforms of its own IR, plus XLA simplification. The API shape to
  copy.
- Slang: `[Differentiable]`, `fwd_diff`, `bwd_diff`, custom derivatives, on the GPU.
- Zygote and Diffractor (Julia): source-to-source AD on a compiler IR.
- egg and egglog: equality saturation. Herbie: float-accuracy-aware rewriting, a model for the
  IEEE-condition rules.
- SymPy and Mathematica: symbolic differentiation and simplification, the bar for readable output.
