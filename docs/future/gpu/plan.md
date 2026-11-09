---
status: plan
summary: Rust kernels in the same crate as the host code, compiled by blaze for Metal, Vulkan, CUDA and the CPU, launched through one typed API.
date: 2026-11-02
---
# GPU kernels in blaze

A user writes a `#[gpu::kernel]` function next to `main`, in the same crate, and launches it on
Metal, Vulkan, CUDA or the CPU with one typed call. blaze compiles host and device code in one
session, so generic kernels, shared structs and the borrow checker work across the boundary.

Nothing here is built yet. The front matter uses the fields `tools/docs.py` checks, so this file
can move under `docs/v2/plan/` unchanged once it's adopted.

The 10-29 decision "no GPU" in [decisions.md](../../v2/plan.md) and
[hardware.md](../../v2/plan/design/data.md) covers blaze's own work. That work is
pointer-chasing and branchy, the build machines have no GPU, and the parts that would map to one
(hashing, compression) already run near memory bandwidth on SIMD. This plan is about a target for
the programs blaze builds, so it doesn't reopen that decision.

## Why blaze and not rustc

rust-gpu (`rustc_codegen_spirv`) and rust-cuda (`rustc_codegen_nvvm`) are rustc backends. Both
build the kernels in a second crate, or compile the crate twice from a `build.rs`. blaze can do
better for three reasons:

1. blaze owns monomorphization. Its collector sees which instances the host launches
   (`scale::<f16>`), so it can send everything reachable from a kernel to a device backend and the
   rest to the native one, in one session. That's the nvcc and SYCL model. A crate built twice
   can't see the host's instantiations, so generic kernels need hand-written instantiation lists.
2. `superset-of-rust` allows an opt-in extension. `#[gpu::kernel]`, `Thread` and `Out<T>` only
   have to work under blaze, and a crate that doesn't use them builds exactly as before.
3. blaze already links LLVM 23 (`vendor/llvm-sys`). That release has SPIR-V, NVPTX and AMDGPU
   backends, so the first device backend is "MIR to LLVM IR with GPU address spaces", reusing the
   native lowering.

`no-rustc` rules out running rust-gpu or rust-cuda as-is. They're still worth reading for prior
art: rust-gpu's control-flow structurizer and its `spirv-std` intrinsics, rust-cuda's address
space handling.

## What a user writes

```rust
use gpu::prelude::*;

#[repr(C)]
#[derive(Copy, Clone, gpu::Pod)]
pub struct Params { pub scale: f32, pub n: u32 }

#[gpu::kernel(threads = 64)]
pub fn scale(t: Thread, p: &Params, input: &[f32], out: Out<f32>) {
    // None for the tail threads past the end, so no `i < n` check to get wrong
    let Some((x, o)) = t.zip(input, out) else { return };
    *o = x * p.scale;
}

// the same kernel for the common shape, with no indices at all
#[gpu::map]
pub fn scale_map(p: &Params, x: f32) -> f32 { x * p.scale }

fn main() -> Result<(), gpu::Error> {
    let dev = gpu::Device::default()?; // Metal on macOS, Vulkan or CUDA elsewhere
    let data: Vec<f32> = (0..1_000_000).map(|i| i as f32).collect();
    let input = dev.upload(&data)?;
    let mut out = dev.alloc::<f32>(data.len())?;
    let p = Params { scale: 2.0, n: data.len() as u32 };
    dev.scope(|s| s.launch(&scale, Grid::cover(data.len()), (&p, &input, &mut out)))?;
    let result = out.download()?; // the scope waited for the GPU, so `out` is free again
    Ok(())
}

#[test]
fn scale_on_cpu() {
    let dev = gpu::Device::cpu();
    // same launch, same kernel, run as Rust on the host
}
```

What the user gets from this:

- `launch` is type-checked against the kernel's signature. `&[f32]` maps to `&Buffer<f32>`, and
  `Out<f32>` to a buffer the launch borrows mutably, so the host can't pass one buffer as both
  `input` and `out`.
- No `descriptor_set` or `binding` numbers. Parameters are numbered in order, and the same list
  produces the host-side layout.
- One definition of `Params` for host and device.
- No data race, out-of-bounds write or use-after-free in safe code. The next section says how.

## Memory safety by construction

The obvious signature, `out: &mut [f32]`, is unsound. Every thread gets a `&mut` to the whole
buffer, which is aliased `&mut`. Nobody notices on a GPU, but the optimizer is allowed to
miscompile it, and a parallel CPU backend really does race. An `Out<T>::at(i)` taking any index is
no better, because two threads can pass the same `i`. The rule: a write's target comes from the
thread's identity, never from an integer the thread computed.

1. **`Thread` is an unforgeable token.** It isn't `Copy` or `Clone`, and the kernel ABI is the
   only thing that creates one. Every safe write goes through it, so "one thread, one slot" holds
   by construction, the way `chunks_mut` and `split_at_mut` prove disjointness for slices.
2. **Partitions are named in the signature.** `Out<T>` means one element per global thread.
   `Out<T, Chunks<4>>` means four contiguous elements per thread, and `Out<T, Tiles<16, 16>>` means
   a 2D tile per workgroup, split among its threads. The launch checks that the buffer's length
   matches the grid and partition, so the `p.n` disagreeing with `out.len()` bug can't happen.
3. **Map, zip and reduce come first.** `#[gpu::map]`, `#[gpu::zip]` and `gpu::reduce` cover most
   kernels with no indices, no bounds checks and no tail handling. The compiler writes the
   indexing, and `#[gpu::kernel]` remains for the cases these don't cover.
4. **Workgroup memory has typed phases.** Threads write their own slot, a barrier hands back a
   read view, and any thread can read any slot of that view:

   ```rust
   let tile: Shared<[f32; 64]> = Shared::new(&t);
   let w = tile.write_own(&t, x);    // WritePhase: own slot only
   let r = w.barrier();              // ReadPhase: every slot, read-only
   let sum = r[0] + r[63];
   ```

   Reading before the barrier, or writing after it, doesn't type-check. That's the most common
   shared-memory race on GPUs.
5. **Uniform control flow is checked.** `barrier()` and subgroup ops inside a branch only some
   threads take are undefined behavior on every GPU. blaze runs a uniformity analysis on MIR, as
   WGSL compilers do, and refuses a divergent barrier with an error naming the kernel and the
   branch.
6. **Scattered writes need atomics or `unsafe`.** A histogram uses `&[AtomicU32]`. A raw `RawOut<T>`
   with `unsafe fn write(i, v)` covers the rest, with the disjointness reasoning in a
   `// SAFETY:` comment.
7. **Host buffers are borrowed until the GPU finishes.** A launch is asynchronous, so freeing or
   reading `out` before it completes is a use-after-free or a race against the device. Launches
   happen inside `dev.scope(|s| ...)`, like `std::thread::scope`, and buffers are borrowed for the
   whole scope. A guard that waits in `Drop` wouldn't be enough, because `mem::forget` skips it
   (the 2015 scoped-thread soundness bug).
8. **Reads stay plain.** Shared `&[T]` is sound as long as nothing writes it during the launch,
   and point 7 guarantees that.

## What NVIDIA's CUDA Rust already does

NVIDIA released CUDA Rust in September 2026 (`github.com/NVIDIA/cuda-rust`, blog post "Introducing
CUDA Rust: Two Tracks for Writing GPU Kernels", 2026-09-08). It's CUDA-only and early alpha, but
it's the closest prior art and already settles several questions above. It has two tracks:

**cuda-oxide (SIMT)** is a rustc codegen backend: MIR, then Pliron (an MLIR-like IR in Rust),
then LLVM, then PTX. Host and `#[kernel]` code share one file and one build, which is the model
this plan wants. Its safety pieces:

- `DisjointSlice<T>` for outputs: each thread gets exclusive access to its own element. This is
  this plan's `Out<T>`.
- `thread::index_1d()` returns a typed index, and `DisjointSlice::get_mut` only takes that type,
  returning an `Option`. This is the `Thread` token idea, with the tail handled by `None`.
- `#[launch_contract(domain = 1, block = (256, 1, 1))]` declares the kernel's indexing.
  `prepare_vecadd(config)` checks a launch config against the contract and the live device limits,
  then returns a proof token the safe launch method requires. A kernel without a contract only
  gets `unsafe` launches. Take this: it makes "the grid matches the indexing" a checked fact.
- Passing the output buffer as an input fails with E0502, the same host-side borrow check this
  plan describes.
- Shared memory still needs `unsafe`, and NVIDIA calls making it safe "active work". The
  phased `Shared<T>` above is a candidate answer.

**cutile-rs (Tile)** runs on stable Rust. A kernel body runs once per tile as one logical thread,
and a JIT through CUDA Tile IR decides how many real threads back it. Its safety pieces:

- `&mut Tensor<f32, {[B]}>` is the tile this block owns. On the host,
  `api::zeros(&[1024]).partition([128])` makes each tile's chunk exclusive, fixes the grid
  (1024 / 128 = 8) and supplies `B`, so the grid can't disagree with the indexing.
- The launch takes ownership of its tensors and hands them back when the GPU finishes. That's
  stronger than borrowing for a scope: nothing on the host can touch a buffer in flight, and no
  `Drop` guard is involved for `mem::forget` to skip. Reusing a partitioned tensor as an input
  fails with E0382.
- No thread indices, shared memory or barriers, so there are no data races within a kernel.
  Users give up control of thread mapping in exchange.
- Every operation is lazy until `.sync_on(&stream)`, so a chain of uploads, launches and
  downloads is one submission.

What this changes in the plan:

1. Adopt launch contracts and proof tokens as the way `launch` checks the grid against the kernel.
2. Offer an ownership-passing launch (`launch(..) -> (out, ..)`) beside `dev.scope`. Scopes are
   better for many launches over long-lived buffers, ownership for pipelines.
3. Treat a tile track as phase 5 or later, where `#[gpu::map]` grows into tiles. Tile IR is
   NVIDIA-only, so on Metal and Vulkan blaze would lower tiles itself.
4. Phased `Shared<T>` and the uniformity check are where this plan goes past cuda-oxide. That's
   worth prototyping early, since NVIDIA says it doesn't have an answer yet.
5. Read cuda-oxide's Pliron dialects and its ecosystem appendix (comparing Rust-GPU, rust-cuda
   and CubeCL) before writing blaze's NVPTX path, and the paper "Fearless Concurrency on the GPU"
   (Elibol, RustConf 2026).

## Where each piece lives

The compiler and `core` own what has to be the same everywhere. A crate owns the drivers.

| Piece | Owner |
|---|---|
| `#[gpu::kernel]`, the kernel ABI, the host/device split in the collector | blaze (compiler) |
| Thread and workgroup ids, `barrier()`, subgroup ops, address spaces | `core::arch::gpu` (blaze's `library/`) |
| What `barrier()` orders, workgroup memory, `Out<T>`'s disjointness rule | memory model, written next to the intrinsics |
| `KernelDesc`: entry name, binary per target, parameter layout | `core`, so every runtime launches every library's kernels |
| `Device`, `Buffer`, queues, `launch`, the Metal, Vulkan, CUDA and CPU backends | a crate (`gpu`), outside std |

Async is the warning. std shipped `Future` with no shared runtime interface, and libraries ended up
tied to tokio. `KernelDesc` in `core` avoids that: a kernel library depends on `core` alone, and the
application picks the runtime. `GlobalAlloc` works the same way.

## Targets

| Target | Path | Notes |
|---|---|---|
| CUDA | MIR, LLVM IR, NVPTX, PTX | Pointers, warp ops and tensor cores all available. |
| Vulkan / OpenCL | MIR, LLVM IR, LLVM SPIR-V backend | LLVM's SPIR-V backend handles OpenCL `Kernel` SPIR-V well. Vulkan `Shader` SPIR-V is still being filled in by clang's HLSL work. Measure before relying on it. |
| Metal | SPIR-V, SPIRV-Cross, MSL, `xcrun metal`, `.metallib` | No MoltenVK. Whatever MSL can't express is a typed error naming the kernel. Emitting AIR (Apple's LLVM dialect) directly would skip a step, but the format is undocumented. |
| WebGPU | SPIR-V, naga, WGSL | Later. naga takes less SPIR-V than SPIRV-Cross. |
| CPU | the native backend | See below. |

`no-fallback` applies to devices too. A kernel that uses something its target can't do, such as a
pointer cast MSL refuses or a subgroup op the device lacks, fails the build with the kernel's path
and the construct. It never runs on the CPU behind the user's back.

## CPU backend

`Device::cpu()` runs the same kernel as host code. A workgroup is one task on blaze's pool. The
threads inside it run as a loop that LLVM can vectorize. Three things make this harder than a loop:

1. **Barriers.** A plain loop runs thread 0 to the end before thread 1 starts, so `barrier()`
   breaks. blaze splits the kernel body at each barrier into "every thread runs part A, then every
   thread runs part B". Values live across a barrier move into a per-thread array. POCL and Intel's
   CPU OpenCL do the same. This is a MIR pass, which is why it lives in the compiler.
2. **Subgroup ops.** A subgroup maps to SIMD lanes, as in ISPC, with one body compiled per lane
   width. An array-based emulation comes first; it's correct but slow, which is fine for tests.
3. **Race checking.** With honest `Out<T>` signatures, the CPU backend can run kernels under Miri
   and ThreadSanitizer, which catches races before the code reaches a GPU.

## Phases

1. `core::arch::gpu` intrinsics, `#[gpu::kernel]` parsing, and `KernelDesc`. CPU backend for
   element-wise kernels (no barriers). A test kernel runs on `Device::cpu()`.
2. NVPTX through LLVM, and the collector split for one session. A generic kernel launched at two
   types runs on CUDA.
3. SPIR-V through LLVM, the Metal path through SPIRV-Cross, and the `gpu` crate's Metal driver
   (`objc2-metal`). The phase 2 kernel runs on a Mac.
4. Barrier splitting and workgroup memory on the CPU. A tiled matmul gives bit-identical results
   on CPU and Metal for integer inputs.
5. Subgroup ops, and SIMD-width compilation on the CPU.
6. Autodiff on MIR (`#[blaze::differentiable]`), the one Slang feature with no Rust equivalent
   on a GPU. `std::autodiff` runs Enzyme on LLVM IR, which device code reaches too late to use.
   Automatic, symbolic and e-graph layers, shared with the CPU, are in
   [../autodiff.md](../autodiff.md).
7. Ray tracing: inline query first, then typed pipelines. See [raytracing.md](raytracing.md).

## Open questions

- Can LLVM 23's SPIR-V backend produce Vulkan compute SPIR-V that SPIRV-Cross accepts for every
  construct phase 3 needs? Or does blaze need its own MIR-to-SPIR-V emitter like rust-gpu's?
- How should `Out<T>` express tiled writes (a workgroup owning a 2D tile) without `unsafe`?
- What does a panic on the device do? A trap plus a per-launch error flag the host reads seems
  right, but bounds checks cost code size, and some kernels will want them off.
- Does the `gpu` crate live in this repo, or in its own repo depending on `core`'s `KernelDesc`?

## Prior art

- rust-gpu (`rustc_codegen_spirv`, `spirv-std`, `cargo-gpu`): Rust to SPIR-V, separate crate.
- rust-cuda (`rustc_codegen_nvvm`, `cust`, `cuda_builder`): Rust to PTX, separate crate.
- NVIDIA CUDA Rust: cuda-oxide (SIMT, single source, `DisjointSlice`, launch contracts) and
  cutile-rs (Tile, partitioned tensors, ownership across the launch). See the section above.
- CubeCL: Rust kernels through a proc macro, with CUDA, ROCm, Metal and Vulkan backends.
- The 2025 Rust GPU demo running one crate on CUDA, Vulkan, Metal, DX12 and WebGPU through both.
- Slang: the bar for ergonomics (automatic bindings, swizzles, `[Differentiable]`, reflection).
- CUDA, SYCL and nvcc: the single-source, one-compiler-session model.
- POCL: barrier splitting for CPU execution of GPU kernels.
- ISPC: subgroups as SIMD lanes.
- Rust's `std::offload` project goal: single-source offload through LLVM. It doesn't cover Metal.
