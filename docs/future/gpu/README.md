---
status: plan
summary: Index of the plans for compiling Rust kernels to GPUs with blaze.
date: 2026-11-02
---
# GPU kernels: the plans

Plans for compiling Rust kernels to GPUs with blaze.

- [plan.md](plan.md): single-source kernels for Metal, Vulkan, CUDA and the CPU, memory safety by construction, NVIDIA CUDA Rust as prior art.
- [shaders.md](shaders.md): vertex and fragment shaders in Rust, MIR to naga IR to MSL, SPIR-V or WGSL, the harness app's Slang ported shader by shader.
- [raytracing.md](raytracing.md): inline ray query, typed pipelines and binding tables, acceleration structures with lifetimes.

Related: [../autodiff.md](../autodiff.md), automatic and symbolic differentiation for CPU and GPU code.
