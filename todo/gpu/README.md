# todo/gpu

Plans for compiling Rust kernels to GPUs with blaze. Each file has the same front matter as `docs/`
(`status`, `summary`, `date`), so it can move there unchanged once adopted.

- [plan.md](plan.md): single-source kernels for Metal, Vulkan, CUDA and the CPU, memory safety by construction, NVIDIA CUDA Rust as prior art.
- [raytracing.md](raytracing.md): inline ray query, typed pipelines and binding tables, acceleration structures with lifetimes.

Related: [../autodiff/plan.md](../autodiff/plan.md), automatic and symbolic differentiation for CPU and GPU code.
