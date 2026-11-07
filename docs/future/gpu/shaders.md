---
status: plan
summary: Vertex and fragment shaders written in Rust and compiled by blaze (MIR to naga IR, then MSL, SPIR-V or WGSL), replacing the harness app's Slang, with a binding table generated from the Rust signatures.
date: 2026-11-06
---
# Shaders in Rust

[plan.md](plan.md) covers compute kernels (`#[gpu::kernel]`). This plan adds graphics stages:
`#[gpu::vertex]` and `#[gpu::fragment]` functions in Rust, compiled by blaze. The first user is the
harness app, whose renderer (`crates/harness/harness-native-render`) draws everything with two
Slang files (`shaders/harness.slang`, 14 entry points, and `shaders/mascot.slang`, 4) through our
slangc fork, then MSL and `xcrun metal`. The goal is to move every one of them to Rust, one shader
at a time, and delete the Slang path.

It decides one thing plan.md left open, and reverses one plan.md assumption: the device backend for
Metal, Vulkan and WebGPU goes from MIR to naga's IR, not through LLVM's SPIR-V backend (see
[The backend](#the-backend-mir-to-naga-ir)). plan.md's NVPTX path through LLVM is unchanged.

## What a user writes

mascot.slang's composite pass, in Rust (`crates/harness/harness-native-shaders/src/mascot.rs`):

```rust
use gpu::prelude::*;

#[repr(C)]
#[derive(Copy, Clone, gpu::Pod)]
pub struct MascotQuad { pub rect: Vec4, pub shape: Vec4, pub color: Vec4 }

#[derive(Copy, Clone, gpu::Varyings)]
pub struct QuadV {
    pub pos: Position, // the clip position out of the vertex stage, the pixel's centre into the fragment stage
    pub uv: Vec2,      // a plain value: interpolated with perspective
}

#[gpu::vertex]
pub fn mascot_quad_vs(vid: VertexIndex, frame: &Frame, quad: &MascotQuad) -> QuadV {
    let c = CORNERS[vid.get() as usize];
    let pt = quad.rect.xy() + c * quad.rect.zw() - frame.place.xy();
    QuadV { pos: Position(vec4(pt.x / frame.view.x * 2.0 - 1.0, 1.0 - pt.y / frame.view.y * 2.0, 0.0, 1.0)), uv: c }
}

#[gpu::fragment]
pub fn mascot_quad_fs(v: QuadV, quad: &MascotQuad, src: &Texture2d, smp: &Sampler) -> Vec4 {
    if quad.shape.y < 0.5 {
        return src.sample(smp, v.uv) * quad.shape.x;
    }
    let d = (v.uv * 2.0 - 1.0).length();
    let a = (1.0 - smoothstep(0.2, 1.0, d)) * quad.color.w * quad.shape.x;
    vec4(quad.color.x * a, quad.color.y * a, quad.color.z * a, a)
}

gpu::entries![mascot_vs, mascot_fs, mascot_quad_vs, mascot_quad_fs];
```

The host binds by name, checked by the compiler, with no index copied by hand and no table parsed
out of generated MSL:

```rust
use harness_native_shaders::mascot;
enc.setVertexBuffer_offset_atIndex(Some(&frame), 0, mascot::mascot_quad_vs::frame.index());
enc.setFragmentTexture_atIndex(Some(&pic), mascot::mascot_quad_fs::src.index());
```

## The API

Everything an entry point says about its interface is a type, read through a trait constant. The
macros only list parameter names and types; they never decide what a parameter means from its
name or its spelling.

| Slang | Rust | Trait |
|---|---|---|
| `[shader("vertex")]`, `[shader("fragment")]` | `#[gpu::vertex]`, `#[gpu::fragment]` | |
| `uint vid : SV_VertexID` | `vid: VertexIndex` | `Param` |
| `uint iid : SV_InstanceID` | `iid: InstanceIndex` | `Param` |
| `float4 pos : SV_Position` (out of vs, into fs) | a `Position` field of the varyings | `Varying` |
| `nointerpolation float3 n : NORMAL` | `n: Flat<Vec3>` | `Varying` |
| `noperspective float2 uv` | `uv: Linear<Vec2>` | `Varying` |
| `float2 uv : TEXCOORD0` | `uv: Vec2` (perspective) | `Varying` |
| the vs output struct, the fs input | `#[derive(gpu::Varyings)] struct` | `Varyings` |
| `ConstantBuffer<Frame> frame` | `frame: &Frame` (`Frame: gpu::Pod`) | `Param` |
| `StructuredBuffer<Inst> insts` | `insts: &[Inst]` | `Param` |
| `Texture2D<float4> tex`, `SamplerState smp` | `tex: &Texture2d`, `smp: &Sampler` | `Param` |
| `float4 f(..) : SV_Target` | `-> Vec4` | `Target` |
| `discard` | `gpu::discard()` (returns `!`) | intrinsic |
| `ddx`, `ddy`, `fwidth`, `tex.Sample*`, `tex.Load` | `dpdx`, `dpdy`, `fwidth`, `Texture2d::{sample, sample_level, load}` | intrinsic |

- **Built-ins are typed parameters.** `VertexIndex` and `InstanceIndex` are newtypes over `u32`
  whose `Param::KIND` is the built-in. A vertex stage that takes no `VertexIndex` doesn't get one,
  and nothing named `vid` is special.
- **Interpolants are a derived struct.** `#[derive(gpu::Varyings)]` writes
  `Varyings::FIELDS`: each field's name and its `<T as Varying>::KIND`. Locations are numbered in
  field order (by `gpu::layout`, below); `Position` takes no location. The vertex stage returns it
  and the fragment stage takes it, so the two can't disagree about a location or a type, which in
  Slang only the semantic strings tie together.
- **Resources are typed parameters**, as in plan.md: `&T` with `T: Pod` is a uniform buffer, `&[T]`
  a read-only storage buffer, `&Texture2d` and `&Sampler` what they say. A resource's slot is never
  written in the shader.
- **The binding table is generated from the signatures.** Each entry macro writes a module named
  like the function, holding `ENTRY: gpu::EntryDesc` (name, stage, parameters with kind and slot)
  and one `gpu::Slot` const per resource parameter, named like it. The host reads
  `mascot::mascot_quad_fs::src`; renaming a parameter breaks the host's build, not its first frame.
  Slots are assigned by one `const fn`, `gpu::layout`: per entry, per Metal argument table (buffer,
  texture, sampler), in parameter order. That function is the one home of the rule: the device
  backend reads the same `ENTRY` constants by const evaluation and hands their slots to naga's MSL
  writer, so the shader and the host can't number differently. It replaces build.rs's parsing of
  `entryPointParams_*` out of Slang's MSL.
- **`gpu::entries![...]`** names the crate's entry points in one static at the crate root,
  `GPU_ENTRIES` (each entry's `EntryDesc` and its function), found by its path, not a symbol, so
  two shader crates in one host don't collide. It is what a device build compiles (below) and what
  a host can list.
- **Math is a library.** `Vec2`, `Vec3`, `Vec4`, `UVec4`, `IVec2` with operators and the swizzles
  the shaders use as methods (`.xy()`, `.zw()`, `.xyz()`), and Slang's free functions under their
  Slang names where they are the same function (`dot`, `normalize`, `lerp`, `smoothstep`,
  `saturate`, `clamp`, `length`), so a port maps line to line. They are plain Rust: on the host they
  run as written, which is what the shader crate's CPU tests run.

### What the compiler adds: two small primitives

`library-over-language` puts everything above in the `gpu` and `gpu-macros` crates. What library
code can't express is two things, and the compiler adds only those:

1. **A device build of a crate.** The crate is built for a device target and its roots are the
   functions `GPU_ENTRIES` names, not `main` or the crate's exports. What is reachable from them
   is collected and lowered by the device backend. This is a build kind next to the proc macro's
   wasm build, not an attribute.
2. **The intrinsics.** Derivatives, sampling, `discard`, and later barriers and subgroup ops, are
   functions in `gpu::intrinsic` with a host body (the CPU meaning, or a panic naming the
   intrinsic where the host has none, like a derivative outside a quad). The device backend lowers
   each of them by its path, from one table in the backend crate, as hir-ty-codegen answers
   `__blaze_cache_code` by name today. Once plan.md's `core::arch::gpu` exists they move there,
   and the table follows.

Nothing in the front end knows a shader stage. The entry's stage and interface are data the backend
reads by const-evaluating `GPU_ENTRIES`, and a crate that doesn't use `gpu` builds exactly as
before (`superset-of-rust`).

### Consistency with plan.md

`#[gpu::kernel]` becomes one more stage of the same machinery: `Thread` is a `Param` whose kind is
the global invocation id, `Out<T>` a `Param` whose kind is a writable storage buffer with
plan.md's partition rule, and `KernelDesc` is `EntryDesc` with `Stage::Compute` and a workgroup
size. One descriptor type serves all three stages, so a runtime that launches kernels also binds
shaders. When `EntryDesc` moves to `core` (plan.md's "`KernelDesc` in core"), `gpu` re-exports it.

## The backend: MIR to naga IR

**Decision: lower MIR straight to naga's IR, validate it with naga's validator, and write MSL,
SPIR-V or WGSL with naga's back ends.** The brief this plan was written from recommended plan.md's
route instead: MIR to LLVM IR, LLVM 23's SPIR-V backend in shader (Vulkan) mode, then SPIR-V to MSL
through naga's `spv-in`, validator and `msl-out`, rather than SPIRV-Cross. That route loses for
three reasons.

1. **There is no MIR to LLVM IR lowering to reuse.** blaze's LLVM backend lowers Cranelift IR
   after ABI lowering: in `crates/hir-ty-codegen/llvm/lower.rs`'s words, "every local is a stack
   slot and every address is an `i64`". Vulkan's logical SPIR-V has neither integer addresses nor
   memory-backed locals, so a shader can't go through that path; plan.md's "MIR to LLVM IR with
   GPU address spaces, reusing the native lowering" needs a new lowering from MIR in any case. A new
   lowering from MIR is the cost of both routes, and the question is only which IR it targets.
2. **Two lossy translators instead of one.** LLVM's SPIR-V backend handles graphics stages only as
   far as clang's HLSL work has driven it: the structurizer (`SPIRVStructurizer`), stage
   inputs and outputs, images and samplers. naga's `spv-in` is naga's least complete front end and
   accepts SPIR-V only in the shapes it knows. A shader that falls into a gap fails in one of them
   with an error naming a SPIR-V id, far from the Rust that caused it. naga IR is structured,
   typed and logical, which is what a shader is, so blaze's lowering states every construct and
   names the Rust function and MIR statement when it can't.
3. **naga's validator is the checker plan.md asks for.** Its uniformity analysis refuses a
   derivative, an implicit-LOD sample or a barrier in non-uniform control flow: plan.md's point 5
   ("uniform control flow is checked") for shaders and kernels alike. Its back ends give MSL (Metal,
   now), SPIR-V (Vulkan), WGSL (WebGPU, plan.md's "later" row), HLSL and GLSL from one module.
   It is pure Rust, so blaze builds it; SPIRV-Cross is C++ and would be the one C++ compiler in the
   path.

What blaze owns in exchange is a **structurizer**: MIR is a control-flow graph, and naga's IR has
only `if`, `loop` (with `break` and `continue` to the innermost loop), `switch` and `return`. MIR
built from Rust source is reducible (Rust has no `goto`; loops, labelled breaks, `?` and `match`
all give reducible graphs), so the structurizer is Ramsey's "Beyond Relooper" (2022) over the
dominator tree, giving nested labelled blocks and loops, then a pass that lowers each branch out of
more than one level into a flag and single-level `break`/`continue`, the only exits naga has. An
irreducible graph is an error naming the function and the edge, never a dispatch loop: a
`loop { switch next { .. } }` would put every block in non-uniform control flow, which naga's
uniformity analysis refuses wherever a derivative or a sample sits, and which GPUs run badly. The
wasm backend's dispatcher (`crates/hir-ty-codegen/wasm.rs`) is the losing design here for that
reason.

The rest of the lowering, MIR to naga:

- Locals become naga `LocalVariable`s of the Rust type's naga type; places become `Access` and
  `AccessIndex` chains; `&` of a local stays a function-space pointer. A raw pointer, an
  integer-to-pointer cast, a `dyn` call, a function pointer call, recursion, an allocation, an
  `i128` or `u128`, and unwinding are typed errors naming the function and the construct
  (`no-fallback`). A panic (a failed bounds check, an `unwrap`) ends the invocation, as
  `panic=abort` ends a thread: `discard` in a fragment stage, a position outside the clip volume
  (so the primitive is culled) in a vertex stage. The device has nowhere to unwind to or report
  to; an entry that opts into blaze's `nopanic` effect makes any reachable panic a compile error
  naming it instead. So a port keeps Slang's `[]` indexing, checked.
- Types: `f32`, `i32`, `u32`, `bool`, `f16` when the target has it; `gpu::Vec*` (a `#[repr(C)]`
  struct the backend knows by the same table as the intrinsics) is a naga vector, so the shaders'
  vector arithmetic is one instruction, not a struct per component. A `#[repr(C)]` struct is a
  naga struct with the layout `core::mem` gives it; a uniform's layout is checked against the
  target's rules (Metal's `float3` is 16 bytes, which is why every field in the harness shaders is
  a `Vec4`) and a mismatch is an error naming the field.
- Every function reachable from an entry becomes a naga function; generic functions are
  monomorphized first, as for any target. blaze's inliner runs first, so small helpers like
  `m_srgb_to_linear` don't survive as calls unless they are big.
- The entry point: its `EntryDesc` says what each argument is. Built-ins become naga
  `Binding::BuiltIn`, varyings fields `Binding::Location` with their interpolation, resources global
  variables with `ResourceBinding { group: 0, binding: i }`, and the MSL writer's per-entry binding
  map takes the slots from `EntryDesc`.

**Where it lives** (`semantics-in-front-end`): the structurizer, the MIR to naga lowering and the
naga calls are codegen, in a new crate, `crates/blaze-shader`, next to hir-ty-codegen and outside
the front end. It reads MIR, layouts and const values through the same queries hir-ty-codegen
uses. The front end gains nothing for phase 1. If the uniformity check is later wanted before
codegen (to report it as a diagnostic on the Rust source in the editor), it moves into a
`hir-ty-*` query crate then.

plan.md's targets table changes accordingly: Metal (MSL), Vulkan (SPIR-V) and WebGPU (WGSL) all go
MIR, naga IR, naga's writer, for kernels too, and SPIRV-Cross leaves the plan. CUDA stays MIR, LLVM
IR, NVPTX, because CUDA kernels want real pointers, which naga doesn't model.

## How a shader crate is built and embedded

plan.md's model is one session: blaze's collector sends what is reachable from a kernel to the
device backend and the rest to the native one, so a host can launch a generic kernel at a type it
picks. That is the goal for shaders too, but it needs the collector split. **Phase 1 is simpler: a
device-only build of a shader crate**, the way proc macros are built for wasm.

- The shader crate (`harness-native-shaders`) is an ordinary library. The host depends on it as
  one, for its types (`MascotFrame`, `MascotQuad`, one definition shared by both sides, as plan.md
  wants) and its binding table, and its tests run its functions on the CPU.
- The host also depends on it as a **gpu artifact**, in cargo's artifact-dependency syntax with a
  kind blaze adds:

  ```toml
  harness-native-shaders = { path = "../harness-native-shaders", artifact = "gpu", lib = true }
  ```

  blaze builds the crate a second time for the device, roots from `GPU_ENTRIES`, and hands the
  dependent crate `BLAZE_GPU_MSL_HARNESS_NATIVE_SHADERS`, the path of the MSL it wrote (all of the
  crate's entries in one source; `_SPV` and `_WGSL` later). The host embeds it with
  `include_str!(env!(..))`. Its key covers everything the MSL depends on: the crate's code, the
  device target, naga's version and options (`keys-cover-every-input`).
- The host compiles the MSL when it starts, with `newLibraryWithSource:options:error:` and
  `MTLMathMode::Fast` (what `xcrun metal -ffast-math` does today). The build then needs no Xcode
  and runs no `xcrun`; Metal's own shader cache makes the second start free. A precompiled
  `.metallib` can come later if the measured start cost asks for it (`slow-is-p0-traced`: a span on
  the compile first).
- A build script never runs blaze or a shader compiler for this: build scripts run sandboxed, and
  a compiler inside one is a second build graph blaze can't see or key.

## Migration and verification

One shader at a time, each on its own switch, with no fallback between them:

1. **Port.** The shader goes into `harness-native-shaders`, each Slang function a Rust function of
   the same name in the same order, so a reviewer reads the two side by side. Slang's `static
   const` arrays become `const`s; `float4` fields stay `Vec4`, so layouts match byte for byte, which
   a test in the shader crate checks against the host's own structs (`harness-native-kip`'s
   `Uniforms` and `model::Vertex` for the mascot).
2. **Test on the CPU** what is hard to verify: the transfer functions (Hegel round trips and the
   ImageIO-measured values the host's hdr.rs already pins), the vertex math against the host's
   expectations. Derivatives and sampling have no CPU meaning until plan.md's CPU backend runs a
   fragment stage in quads; their host bodies panic naming themselves.
3. **Switch.** build.rs keeps `SLANG`, the list of Slang shaders it compiles. A shader moves from
   that list to the Rust side when its device build exists; the host's pass for it then loads the
   Rust MSL and binds through the generated slots. If the Rust path can't build, the build fails
   naming the entry and the construct; it never quietly takes the Slang one.
4. **Shots match.** An offscreen shot (`crates/harness/harness-native/src/shot.rs`) of the same
   scene before and after the switch, as PNG and as PFM (`HARNESS_NATIVE_SHOT_PFM`, the unclipped
   float frame), compared pixel by pixel. Fast math lets the two compilers differ in the last bits,
   so the bound is the PFM's largest absolute difference (1/1024 of white, to start), and any pixel
   past it fails with its position. For the mascot the scene is a genui cell showing Kip
   (`HARNESS_NATIVE_GALLERY=genui`, `HARNESS_NATIVE_GENUI=<tree.json>`), at a fixed time
   (`HARNESS_NATIVE_SHOT_SECS`) so his pose is the same in both. Both shots and the tree are kept
   with the comparison (`measure-the-exact-tree`).
5. **Delete.** When the last shader leaves `SLANG`, build.rs, the slangc lookup and the MSL parser
   go, and so does the Slang dependency.

## Phases

1. **The vocabulary and the ports.** The `gpu` crate (types, traits, `layout`, the math, the
   intrinsics' host bodies), `gpu-macros` (`vertex`, `fragment`, `Varyings`, `Pod`, `entries!`),
   `harness-native-shaders` with mascot.slang ported, its CPU tests and its layout tests against
   the host. The structurizer in `blaze-shader`, with a Hegel test that runs random reducible
   graphs and their structured form under the same branch choices and compares the blocks they
   visit. (This lane.)
2. **MIR to naga for the mascot.** The lowering for what mascot.slang uses (scalars, vectors,
   structs, arrays, uniform and storage buffers, a texture and a sampler, `fwidth`, `pow`, `if`),
   the device build kind and `artifact = "gpu"`, MSL out. Kip's pass loads it; shots match; mascot
   leaves `SLANG`.
3. **harness.slang.** Port the 14 entry points (SDFs, the HDR transfer functions, Slug's curve
   bands, `asuint`, loops, `SampleLevel`, `Load`, `discard`); each pair leaves `SLANG` when its
   shots match. Then the Slang path is deleted.
4. **SPIR-V and WGSL out**, and the `gpu` crate's Vulkan driver, so the harness renderer can run
   off macOS.
5. **One session**: plan.md's collector split, so a host can use a generic shader at a type it
   picks and the artifact dependency goes away. Kernels (plan.md phases 1 to 3) share the same
   descriptor, intrinsics table and backend.

## Open questions

- Where does `gpu` live once kernels and drivers join it: one crate with the drivers behind a
  feature, or the vocabulary (`gpu`) and the drivers (`gpu-metal`, ...) apart? The vocabulary may
  use std (its float functions call the intrinsics the device backend lowers, `powf32` and the
  rest), but nothing in it may reach an allocation or a driver, since a device build collects
  whatever an entry reaches.
- Should a panic also be reported? Ending the invocation (above) is silent; a per-draw error
  buffer the host reads after the frame (plan.md's per-launch flag) would say which entry and
  where, at the cost of a storage binding on every entry. Opt in per build (debug), or always?
- Is MIR always reducible? Rust gives no `goto`, but MIR passes (jump threading, early returns
  through drop elaboration) could in principle make an irreducible graph. The structurizer refuses
  one with an error; if that ever fires on real code, node splitting is the fix.
- naga's version is a key input of every device artifact. Pin it in the workspace and bump it
  deliberately, like the toolchain.
- Does blaze's device build need its own target triple (`gpu-naga-unknown`?), or is the device a
  property of the build kind with no `cfg(target_*)` visible to the crate? A triple lets a shader
  crate `cfg` on it, which shaders should rarely need.

## Prior art

- rust-gpu (`spirv-std`, `#[spirv(vertex)]`): built-ins as attributes on parameters
  (`#[spirv(vertex_index)] vid: u32`), a separate crate built by `spirv-builder` from a build
  script. This plan makes built-ins types instead and keeps build scripts out of it.
- naga (wgpu's shader translator): the IR, validator and back ends this plan uses.
- Slang: reflection of entry-point parameters into a binding table, which build.rs approximates
  today by parsing MSL; here the table is Rust constants.
- WGSL: `@builtin(position)` on a struct field, `@interpolate(flat)` on a field: the varyings
  struct follows it, with the attributes as types.
- Norman Ramsey, "Beyond Relooper: recursive translation of unstructured control flow to
  structured control flow" (ICFP 2022): the structurizer.
