# todo

Plans for work not started yet. Each plan has the same front matter as `docs/` (`status`, `summary`, `date`), so it can move under `docs/v2/plan/` unchanged once adopted.

- [gpu/](gpu/README.md): Rust kernels compiled by blaze for Metal, Vulkan, CUDA and the CPU.
  - [gpu/plan.md](gpu/plan.md): single-source kernels, memory safety by construction, NVIDIA CUDA Rust as prior art.
  - [gpu/raytracing.md](gpu/raytracing.md): inline ray query, typed pipelines, acceleration structures with lifetimes.
- [autodiff/plan.md](autodiff/plan.md): automatic differentiation on MIR, symbolic derivatives of pure functions, e-graph simplification.
- [capabilities/plan.md](capabilities/plan.md): edition `blaze-2026.11`, where std has no ambient authority, `main` receives `Io`, code is safe Rust, and signatures are permissions.
- [bootstrap/plan.md](bootstrap/plan.md): bootstrap blaze from a wasm seed (its source plus itself compiled to wasm), so no rustc is needed to build blaze from nothing.
