---
status: plan
summary: Vertex and fragment shaders, and compute kernels with inline ray query, written in Rust and compiled by blaze (MIR to naga IR, then MSL, SPIR-V or WGSL), replacing the harness app's Slang, with a binding table generated from the Rust signatures.
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
| `SV_Position` read in a fragment (the pixel's centre) | `v.pos.0` | `Varying` |
| `asuint(x)` | `asuint(x)` (`f32::to_bits`) | library |
| `frac`, `sign` | `frac`, `sign` (`sign` is -1, 0 or 1, 0 for 0 and NaN, as Slang's) | library |
| `lerp(a, b, t)` with a vector `t` | `lerp(a, b, t)` (per component) | library |
| `int4`, `int2(v)`, `float2(v)`, `float3(v, z)` | `IVec4`, `v.as_ivec2()` (toward zero), `v.as_vec2()`, `v.extend(z)` | library |
| `clamp` on an integer vector | `IVec2::clamp`, as on every vector | library |
| `out float2 p` | `p: &mut Vec2` | language |
| `select(c, a, b)` on vectors | the scalar `if` per component | language |
| `for`, `break` | Rust's `for` over a range, `break` | language |

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
- **Math is a library.** `Vec2`, `Vec3`, `Vec4`, `UVec4`, `IVec2`, `IVec4` with operators and the
  swizzles the shaders use as methods (`.xy()`, `.zw()`, `.xyz()`, `.yx()`), `clamp` on every
  vector, conversions as methods (`as_ivec2`, `as_vec2`, `extend`), and Slang's free functions under
  their Slang names where they are the same function (`dot`, `normalize`, `lerp`, `smoothstep`,
  `saturate`, `clamp`, `length`, `frac`, `sign`, `asuint`), so a port maps line to line. Slang
  constructs that Rust has its own spelling for stay Rust: an `out` parameter is `&mut`, a vector
  `select` is the scalar `if` per component, a loop is `for` over a range. Integer division by zero
  and overflow panic in Rust where Slang leaves them undefined; on the device a panic ends the
  invocation. They are plain Rust: on the host they
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
- Every call reachable from an entry is inlined into it (phase 2's decision; see "Phase 2 as
  built"): each callee's monomorphized MIR, chosen by the code generator's own dispatch, is a
  fresh frame of one flat graph. A panic at any depth is then one branch to the end of the
  invocation, a reference never crosses a naga call, and resources stay globals. Recursion is
  refused. Whether big helpers should become naga functions again is a size question for later,
  with a span on the MSL size first.
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

## Phase 2 as built

What `crates/blaze-shader` does now, and the choices it made where the plan above was open.

- **Pipeline.** `device::device_msl(db, krate)` reads `GPU_ENTRIES` by const evaluation
  (`entries.rs`), lowers each entry into one naga module (`lower.rs`), validates the module with
  `ValidationFlags::all()`, and writes MSL 2.4 with naga's writer. Every refusal in every entry
  comes back at once, each naming the entry, the function, the construct and the rule
  (`refuse.rs`).
- **One home for each decision** (`mirrors-derive-or-check`):
  - bodies come from `dispatch::body_of`;
  - callees come from `dispatch::call`, the code generator's dispatch;
  - layouts come from `layout_of_ty`: a struct's members sit at the front end's offsets, an array
    has its stride, and a uniform's size and a storage stride are checked against the descriptor;
  - enum discriminants come from `const_eval_discriminant`, and constant bytes are decoded with
    `detect_variant_from_bytes`;
  - which functions panic comes from `hir_ty_pure::models`;
  - slots come from the `EntryDesc`s, and the storage sizes buffer from `gpu::sizes_slot`.

  The intrinsics table (`intrinsics.rs`) is the one place that lowers anything by name: the gpu
  crate's vectors, textures and device functions; core's intrinsics; and std's libm calls
  (`atan2f` and kin), so std's own `f32` bodies are still what is lowered.
- **Runtime MIR, not codegen's optimized MIR.** The lowering reads the same monomorphized bodies
  the code generator starts from. naga and Metal optimize after.
- **References are places, not values.** A reference or raw pointer local is bound once, as the
  graph is built, to a symbolic place: a local, a resource or a place of no size, plus field steps
  and index steps (an index is snapshotted where the reference is taken). Pointer casts that keep
  the pointee keep the place, and `&[T; N]` to `&[T]` records `N`. A reference that names
  different places on different paths is refused.
- **Panics.**
  - Blocks from which every path ends in a panic, `unreachable` or an abort are pruned, and edges
    into them, like a failed `assert`, go to the end of the invocation.
  - A fragment is discarded; a vertex returns zeroed varyings at clip position (2, 2, 2, 1).
  - `ub_checks()` is false, and the branch it guards is pruned before the graph is walked, so
    core's precondition checks (`const_eval_select`, pointer arithmetic) are never lowered.
- **Enums** (needed by `for` loops, through `Option`) lower as values: a naga struct holding the
  discriminant then every variant's fields, a layout of the device's own. A uniform or storage
  type holding an enum is refused, since the host would write other bytes.
- **The device target.**
  - The device build is a target of its own, `naga-unknown-unknown`: `target_arch = "naga"`,
    32-bit `usize`, little endian, `panic = "abort"`, data layout `e-m:e-p:32:32-i64:64-n32-S128`.
  - The toolchain's core and std are built for it like wasm32-unknown-unknown's (std's
    unsupported platform layer), so a shader can call std's `f32` methods.
  - A triple, not only a build kind, because core's MIR depends on pointer width and endianness,
    and those are target properties. A shader crate may `cfg(target_arch = "naga")`, which it
    should rarely need.
- **The output.** The dependent crate gets `BLAZE_GPU_MSL_<CRATE>`: the crate's name upper-cased
  with `-` as `_`, holding the path of the crate's one MSL source. Its key is the crate's code, the
  device target, naga's pinned version and the writer's options (`keys-cover-every-input`).
- **The Metal step.**
  - No build step runs `xcrun`, and nothing leaves the sandbox. The host compiles the MSL at
    start with `newLibraryWithSource:options:error:` and `MTLMathMode::Fast`.
  - A `.metallib` step, if a span on the start-up compile ever asks for one, runs only in the
    daemon, outside the sandbox, as an explicit opt-in step. It is keyed by the MSL's bytes, the
    output of `xcrun metal --version` and the flags, and stops with an error when Xcode's Metal
    toolchain is missing.
- **Left for phase 3**, and what phase 3 did of it (next section): the target spec,
  `artifact = "gpu"` and the env var; the mascot's switch; an end-to-end test. Still left:
  uniformity diagnostics on the Rust source; vector fast paths, so `gpu::vec` operators lower as
  one naga vector op rather than per component (correct through their Rust bodies now);
  derived-impl methods (`Callee::Derived`, e.g. a derived `PartialEq` call), slices of a local
  array passed through `Index`, `dyn`, closures.

## Phase 3 as built

How `artifact = "gpu"` goes through the build, crate by crate, as the daemon runs it.

- **The manifest** (blaze-manifest `dependency.rs`). `artifact = "gpu"` is a kind of blaze's own:
  it takes no other kind and no `target`, it needs no `-Z bindeps`, and it is refused as a
  build dependency, naming the dependency. `lib = true` keeps the ordinary library edge beside
  it.
- **The unit graph** (blaze-units `graph.rs`). A fourth kind, `Kind::Gpu`, builds for
  `GPU_DEVICE` (`naga-unknown-unknown`, `Request::gpu`). The dependency becomes one `Build` unit
  for the device, under every goal, on an edge marked `gpu` (`UnitDep::gpu`). The device unit
  depends on the device's own `core`, std and `gpu` crate. With `--target naga-unknown-unknown`
  the gpu kind is the target kind. A proc macro, or a graph with no device platform, refuses the
  dependency, naming both crates.
- **The plan** (`load/plan.rs`). Device units abort on panic, and the device's std is planned
  with no rustflags. The device unit's plan names `<deps>/<crate><extra-filename>.msl`
  (`UnitPlan::gpu_msl`, also among its outputs). Each dependent's compile gets
  `BLAZE_GPU_MSL_<CRATE>` naming that file (`CompileEnv::gpu_msls`). The record key covers it all:
  - the plan, whole: the device target, the path, the profile;
  - the shader crate's sources, through its own unit;
  - naga =30.0.1 and the writer's options (`MSL_VERSION`, no fake bindings): these are code of
    blaze-shader, which is in blaze-cargo's record code identity (`blaze_codegen_code::RECORD_ROOTS`,
    with no cut).
- **The front end** (blaze-lower). The gpu edge gives the dependent no extern crate.
  `Lowered::gpu_msl(unit, bytes)` sets the device unit's MSL as the bytes of that file, through
  `base_db::Db::set_included_file`. So the host's `include_str!(env!("BLAZE_GPU_MSL_<CRATE>"))`
  reads what the device build wrote, before and whatever placement puts on disk. A record that
  read the file is filed by those bytes (`Files::included_by`).
- **The daemon** (blaze-cargo). A device unit (`Unit::for_device`) builds no native code, and is
  linked under every goal (`checks_only` is false, and check and build share its record key), so a
  checked host reads the MSL that a built one does. Its link (`compile::assemble`) does two things:
  - writes the crate record;
  - at `gpu_msl`, writes `blaze_shader::device::device_msl` of its crate. A refusal fails the
    unit, naming every construct, and nothing else is tried.

  `Compiled::gpu_msl`, made or read back from a kept record, goes to the job's `Writer::gpu_msl`.
  That is applied in the runs' batch and counted as a run (`graph::flag::GPU_MSL`), so the
  dependents wait for it as they wait for a build script's run.
- **Tests.**
  - blaze-units `load/tests.rs`: the plan of a gpu dependency (one device unit, abort, the MSL
    among its outputs, the env var on the host, the edges with and without `lib = true`) and what
    it refuses.
  - blaze-cargo `serve/unit/tests/gpu.rs`, the end-to-end test, on a shader crate and a host over
    minicore: the host waits on the MSL; the MSL is naga-validated and the same bytes from two
    front ends (rule 25); the host's `include_bytes!` reads exactly those bytes once applied, and
    an edit's after; a unit that writes no MSL is refused, naming it.
- **Not wired yet.**
  - `blaze metadata --unit-graph` refuses a gpu dependency (no `Request::gpu`).
  - Feature resolution does not resolve features for the device kind on its own, so a dependency
    only under `cfg(target_arch = "naga")` gets the host's features.
  - The device build lowers every entry of `GPU_ENTRIES`, so a crate with Slang still in `SLANG`
    must lower all of its entries before any of its MSL is written. For harness-native-shaders that
    includes the 14 harness entries.

## Compute kernels and inline ray query (phase 3b)

The first user is orrery's light pass (indexable-inc/orrery, `crates/orrery-web/shaders/light.slang`):
a compute kernel, one thread a device pixel, that casts an orthographic ray per pixel with an inline
ray query, picks a window light by resampled importance, casts a shadow ray, and keeps a running
mean per pixel as packed halves in a read-write buffer. It runs on Metal through wgpu's MSL
passthrough. This is the third stage of the one descriptor (as "Consistency with plan.md" says)
and the first piece of [raytracing.md](raytracing.md)'s inline query, scoped to what that pass
needs; plan.md's `Out<T>` partitions and `Shared<T>` phases come later on the same machinery.

```rust
use gpu::prelude::*;

#[gpu::kernel(threads = (8, 8))]
pub fn light_cs(t: Thread, view: &LightView, scene: &Tlas, insts: &[Inst], acc: &RawOut<UVec2>) {
    let id = t.global();
    if id.x >= view.region.z || id.y >= view.region.w {
        return;
    }
    let at = (id.y * view.region.z + id.x) as usize;
    let ray = Ray { origin: o, dir, tmin: 0.0, tmax: 1e7 };
    let Some(hit) = scene.trace(ray, 0xff) else {
        // SAFETY: each thread writes only its own pixel, `at`, which no other thread computes.
        unsafe { acc.write(at, uvec2(0, 0)) };
        return;
    };
    let inst = insts[hit.instance_index as usize];
    let lit = !scene.occluded(Ray { origin: from, dir: to, tmin: 0.0, tmax: len }, 0xff);
    // SAFETY: as above.
    let before = unsafe { acc.read(at) };
    // ...
}
```

| Slang | Rust | Kind |
|---|---|---|
| `[shader("compute")] [numthreads(8, 8, 1)]` | `#[gpu::kernel(threads = (8, 8))]` (`threads = 64`, `(x, y)` or `(x, y, z)`) | `Stage::Compute { x, y, z }` |
| `uint3 id : SV_DispatchThreadID`, `SV_GroupThreadID`, `SV_GroupID` | `t: Thread`, then `t.global()`, `t.local()`, `t.group()` (each a `UVec3`) | `Kind::Thread` |
| `RWStructuredBuffer<T> acc` | `acc: &RawOut<T>`, `unsafe { acc.read(i) }`, `unsafe { acc.write(i, v) }`, `acc.len()` | `Kind::RawOut { stride }` |
| `RaytracingAccelerationStructure scene` | `scene: &Tlas` | `Kind::Tlas` |
| `RayDesc` | `Ray { origin, dir, tmin, tmax }` | library |
| `RayQuery<FORCE_OPAQUE>`, `TraceRayInline`, `while (q.Proceed()) {}`, `CommittedStatus() == COMMITTED_TRIANGLE_HIT`, `CommittedRayT()`, `CommittedInstanceIndex()`, `CommittedPrimitiveIndex()` | `scene.trace(ray, mask) -> Option<Hit>`; `hit.t`, `hit.instance_index`, `hit.primitive_index`, `hit.instance_custom_index`, `hit.geometry_index`, `hit.barycentrics`, `hit.front_face` | library over one intrinsic |
| the same with `ACCEPT_FIRST_HIT_AND_END_SEARCH`, `CommittedStatus() == COMMITTED_NOTHING` | `scene.occluded(ray, mask) -> bool` | library over one intrinsic |
| `f32tof16(x)`, `f16tof32(u)` | `f32tof16(x)`, `f16tof32(u)` (Slang's names; round to nearest even, the low 16 bits) | intrinsic with a host body |
| `uint2`, `uint3` | `UVec2`, `UVec3` | library |

- **A kernel returns nothing** and takes no `VertexIndex`, `InstanceIndex` or varyings; `Thread`
  and `RawOut` are a kernel's alone. `gpu::check` refuses each at compile time with the rule, as it
  does for the graphics stages, and refuses a workgroup of no thread or of more than Metal's 1024.
  The workgroup size is in the descriptor (`ENTRY.stage`), so the host dispatches by the same
  numbers the shader was compiled with; wgpu's MSL passthrough takes it from there.
- **`Thread` is the thread's identity**, made only by the kernel ABI: not `Copy` or `Clone`, with no
  public constructor, as plan.md says, so the safe write paths plan.md adds later can key on it.
  On the device it is three built-in arguments (global, local and workgroup invocation ids).
- **Writes are `unsafe` and named.** plan.md's rule: `&mut [T]` would hand every thread a `&mut` to
  one buffer, so it is not a `Param` (the trait's `on_unimplemented` names `RawOut`). `RawOut<T>`
  is plan.md's raw writer: `read` and `write` check the index against `len()` (a failed check ends
  the invocation, like every panic) and are `unsafe` for the one thing a check can't see, two
  threads touching one element. The light pass's `SAFETY:` is that each thread owns its pixel.
  Like `Texture2d`, a `RawOut` and a `Tlas` exist only on the device: their methods' host bodies
  panic naming themselves until plan.md's CPU backend.
- **Inline query, opaque only.** `trace` runs the whole query (initialize, proceed until done,
  read the committed hit) with `FORCE_OPAQUE`, so every triangle the ray meets is a candidate the
  hardware commits itself and no candidate loop is needed; `occluded` adds
  `ACCEPT_FIRST_HIT_AND_END_SEARCH` and says whether anything was committed. The query object never
  appears in Rust, so it can't be moved, copied or read before it is initialized. Both are Rust
  over one private intrinsic, `Tlas::query(ray, flags, mask)`, which returns the committed
  intersection as a plain struct read member by member by name; `Option<Hit>` is built in Rust,
  so the backend never composes an enum. raytracing.md's
  `RayQuery` with `Candidate` arms, `commit` and `commit_at` (alpha-tested and procedural
  geometry) is the next step and adds to this; it replaces nothing.
- **Slots.** Metal binds an acceleration structure in the buffer table, so `&T`, `&[T]`,
  `&RawOut<T>` and `&Tlas` take the next buffer slot in parameter order, textures and samplers
  their own tables, and the storage sizes buffer the slot after the last buffer, counting `&[T]`
  and `&RawOut<T>` (`gpu::layout` and `gpu::sizes_slot`, unchanged in shape). wgpu's Metal backend
  numbers a bind group's buffers and acceleration structures together in entry order, so a host
  that lays its bind group entries in parameter order, with the sizes buffer as a uniform after
  them, binds what the MSL reads.
- **The module's capabilities** are what it uses: the validator gets `RAY_QUERY` when an entry
  traces and `SHADER_FLOAT16_IN_FLOAT32` when it packs halves (Metal has both), nothing else
  beyond the base set.

What is left after it: `Out<T>` and its partitions, `Shared<T>`, barriers and atomics (plan.md);
`RayQuery` with candidates (raytracing.md); a kernel on the CPU.

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
   (`HARNESS_NATIVE_GALLERY=genui`, `HARNESS_NATIVE_GENUI=<tree.json>`), under Reduce Motion
   (`HARNESS_NATIVE_SHOT_REDUCE_MOTION`) so his pose is the same in both: the shot's clock is the
   wall clock, so a fixed `HARNESS_NATIVE_SHOT_SECS` alone leaves his breath where the frames fell.
   Both shots and the trees are kept with the comparison (`measure-the-exact-tree`). The commands
   are in [Verifying the port](#verifying-the-port).
5. **Delete.** When the last shader leaves `SLANG`, build.rs, the slangc lookup and the MSL parser
   go, and so does the Slang dependency.

## Verifying the port

The mascot's comparison, as commands. The Slang side is the tree just before the switch (the commit
that deletes `shaders/mascot.slang`), with Kip's Reduce Motion fix applied, since that tree
predates it. The Rust side is the tree after the switch. Both build with blaze (rule 2), and both
run harness.slang through build.rs as before, so only the mascot's shaders differ. The `blaze` on
`PATH` is one built from a tree with this phase in it: the Rust side's manifest needs
`artifact = "gpu"`. The same binary builds both sides.

```sh
rust=/Volumes/Projects/blaze-rust-shaders-p3-1109   # the Rust mascot's tree
slang=/Volumes/Projects/blaze-kip-slang             # the Slang mascot's, made here
out=/Volumes/Projects/kip-shots/$(date +%Y%m%d-%H%M%S)
switch=$(git -C "$rust" log -1 --format=%H --diff-filter=D -- crates/harness/harness-native-render/shaders/mascot.slang)
git -C "$rust" worktree add --detach "$slang" "$switch^"
# Kip's Reduce Motion fix and the shot's switch for it, onto the Slang tree: neither file is touched
# by the switch, so their diff from its parent is the fix alone.
git -C "$rust" diff "$switch^" HEAD -- crates/harness/harness-native-render/src/kips.rs \
    crates/harness/harness-native/src/shot.rs | git -C "$slang" apply
mkdir -p "$out"
for tree in "$slang" "$rust"; do
    side=$([ "$tree" = "$slang" ] && echo slang || echo rust)
    git -C "$tree" rev-parse HEAD > "$out/$side.commit"
    git -C "$tree" diff HEAD > "$out/$side.diff"     # the Slang tree's applied fix; empty for Rust
    blaze build --release --manifest-path "$tree/crates/harness/Cargo.toml" -p harness-native
done
printf '{"type":"Kip","props":{"mood":"idle","size":160},"children":[]}' > "$out/kip.json"
for tree in "$slang" "$rust"; do
    side=$([ "$tree" = "$slang" ] && echo slang || echo rust)
    for look in dark light; do
        HARNESS_NATIVE_GALLERY=genui HARNESS_NATIVE_GENUI="$out/kip.json" \
        HARNESS_NATIVE_SHOT_REDUCE_MOTION=1 HARNESS_NATIVE_SHOT_SECS=0.8 HARNESS_NATIVE_SHOT_BACKING=2 \
        HARNESS_NATIVE_APPEARANCE=$look \
        HARNESS_NATIVE_SHOT_PFM="$out/$side-$look.pfm" HARNESS_NATIVE_SHOT="$out/$side-$look.png" \
            "$tree/crates/harness/target/release/harness-native"
    done
done
for look in dark light; do
    python3 -I "$rust/harness/pfmdiff.py" "{\"before\":\"$out/slang-$look.pfm\",\"after\":\"$out/rust-$look.pfm\"}"
done
```

What the commands hold fixed, and why:

- **The pose.** Under Reduce Motion Kip rests still, so every mood shows the same rest pose, and
  only a blink moves him. The first blink comes no sooner than `kip_blink_min` (2.4 s, kip.flecs)
  after he is first painted, so a shot at 0.8 s has none. The pointer is not over him, so he looks
  straight ahead. A shot without Reduce Motion is not comparable pixel by pixel: the shot's clock
  is the wall clock, so breath and glances land where the frames fell.
- **The two appearances.** Dark and light give the shadow another colour (`look.shadow.at(dark)`),
  and the premultiplied picture another backdrop to blend over, so both are drawn.
- **The bound: 1/1024 of white on the PFM's largest absolute difference** (pfmdiff.py's default).
  - Both sides compile under fast math: Slang's MSL by `xcrun metal -ffast-math`, the Rust side's
    by `newLibraryWithSource` with `MTLMathMode::Fast`. Fast math lets a compiler reassociate,
    fuse multiply-adds and approximate `pow`, `exp` and `sqrt`, so two sources of the same
    arithmetic differ in their last bits, and an `fwidth`-smoothed edge can turn that into a few
    units in the last place across a pixel.
  - Compiler noise sits far below 1/1024, and a real difference sits above it. 1/1024 is a quarter
    of one 8-bit sRGB step (1/255), so any change a PNG can show fails: a wrong constant, a swapped
    operand, a slot bound to the wrong buffer, a dropped `discard`.
  - The PFM is compared, not the PNG, because the PNG clips past white and rounds to 8 bits, and
    either can hide a difference.
- **What is kept.** `$out` holds both trees' commits and the Slang tree's applied diff, the tree
  JSON, and both sides' PNGs and PFMs. A failure is a pixel list (position, channel, both values)
  in pfmdiff's output. Remove the Slang worktree after (`git -C "$rust" worktree remove "$slang"`).

The animated moods are a look, not a bound. For each of `idle thinking working happy confused
sleeping wave`, take a shot without `HARNESS_NATIVE_SHOT_REDUCE_MOTION`, put the two sides next to
each other, and check that nothing is wrong in shape, colour or outline. Any difference past
breath and timing is a bug to trace with the bounded shot above.

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
   shots match. Then the Slang path is deleted. The port is written
   (`harness-native-shaders/src/harness.rs`, with CPU tests in `harness_tests.rs`, and
   harness-native-render's `shader_port.rs` checking its structs against pack.rs, its parameters
   against the MSL's `BINDINGS` and its curves against hdr.rs); the lowering for loops, slices
   passed to helpers, `&mut` out parameters, local arrays indexed at run time, integer vectors and
   the sampling intrinsics is what is left.
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
- ~~Does blaze's device build need its own target triple?~~ Decided in phase 2: yes,
  `naga-unknown-unknown` (see "Phase 2 as built").

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
