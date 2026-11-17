---
status: plan
summary: Ray tracing in blaze GPU kernels, built on inline ray query, typed pipeline payloads and acceleration structures that carry lifetimes.
date: 2026-11-02
---
# Ray tracing

This builds on [plan.md](plan.md): the same `#[gpu::kernel]`, the same one-session compile, the
same `Device` backends. The first piece is being built: an opaque-only inline query on Metal
(`Tlas::trace` and `Tlas::occluded`), in [shaders.md](shaders.md#compute-kernels-and-inline-ray-query-phase-3b).

## What each API offers

| API | Inline query from any kernel | Pipeline (raygen, hit, miss stages) |
|---|---|---|
| Vulkan | `VK_KHR_ray_query` (`OpRayQuery*` in SPIR-V) | `VK_KHR_ray_tracing_pipeline` with a shader binding table |
| DX12 | DXR 1.1 `RayQuery` | DXR 1.0 with a shader table |
| Metal | `intersection_query` (MSL 2.4+) and `intersector<>` | intersection and visible function tables |
| CUDA | none | OptiX: programs per stage, binding table, a library on top of PTX |
| CPU | Embree's `rtcIntersect` / `rtcOccluded` or a BVH of blaze's own | n/a |

Inline query is the common piece. Vulkan, DX12 and Metal run the same loop: start a query,
`proceed` through candidate hits, `commit` the ones the program accepts, and read the committed
hit. CUDA is the odd one out: OptiX has only the pipeline.

## Inline query first

`core::arch::gpu` gets one query type that every backend lowers:

```rust
#[gpu::kernel(threads = 64)]
pub fn shade(t: Thread, scene: &Tlas, rays: &[Ray], mats: &[Material], out: Out<Rgba>) {
    let Some((ray, o)) = t.zip(rays, out) else { return };
    let mut q = RayQuery::new(scene, *ray, RayFlags::empty());
    while let Some(c) = q.proceed() {
        match c {
            Candidate::Triangle(tri) => {
                if mats[tri.instance_id() as usize].alpha_at(tri.barycentrics()) > 0.5 {
                    q.commit();
                }
            }
            Candidate::Procedural(p) => {
                if let Some(d) = sphere_hit(ray, p.primitive_id()) { q.commit_at(d) }
            }
        }
    }
    *o = match q.committed() {
        Some(hit) => mats[hit.instance_id() as usize].shade(&hit),
        None => SKY,
    };
}
```

How each backend lowers it:

- **Vulkan** lowers to `OpRayQueryInitializeKHR`, `OpRayQueryProceedKHR` and the rest, either from
  the LLVM SPIR-V backend or from blaze's own emitter.
- **Metal** lowers to SPIRV-Cross's translation to `intersection_query`. If SPIRV-Cross can't do
  it, blaze emits MSL for the query itself.
- **CUDA** has no inline query, so blaze turns the loop into OptiX programs. The `Candidate` arms
  become any-hit and intersection programs, the code after the loop becomes closest-hit and miss,
  and values live across the loop go into the payload. It's the same kind of MIR split as barrier
  splitting in [plan.md](plan.md#cpu-backend). A loop body the split can't handle, such as one
  that writes to `out` mid-loop, is a typed error naming the kernel. It never silently runs
  somewhere slower.
- **The CPU** uses Embree's query API, or a BVH blaze owns, under the same loop.

### As built first: opaque queries through naga

The backend is shaders.md's (MIR to naga IR, then naga's MSL writer, which writes Metal's
`intersection_query`), not SPIR-V or SPIRV-Cross, so the Metal row above comes before the Vulkan
one. The first API is the one a query loop with no candidate work reduces to:

```rust
pub fn trace(&self, ray: Ray, mask: u32) -> Option<Hit>; // FORCE_OPAQUE, the closest hit
pub fn occluded(&self, ray: Ray, mask: u32) -> bool;     // FORCE_OPAQUE | ACCEPT_FIRST_HIT_AND_END_SEARCH
```

Both are Rust over one intrinsic, `Tlas::query(ray, flags, mask)`, which lowers to one naga
query in a local variable: `Initialize` with the ray, a `loop` of
`Proceed` that breaks when it returns false, and `RayQueryGetIntersection { committed: true }`,
read into a plain struct by member name; its `kind` (none or triangle, the only kinds an opaque
query commits) picks `None` or `Some` in Rust.
`Hit` is a plain Rust struct of the committed intersection's fields (`t`, `instance_index`,
`instance_custom_index`, `geometry_index`, `primitive_index`, `barycentrics`, `front_face`); the
object-to-world matrices are left out until a user needs them. `RayQuery::new` and `proceed` with
`Candidate` arms (above) come next, for alpha-tested and procedural geometry.

## Pipeline second

The pipeline model pays off for big scenes with many materials, because the hardware schedules
the hit shaders. Its classic bugs are a shader binding table whose layout disagrees with the
shaders, and a payload whose size or layout differs between `trace` and the hit shader. Types
remove both:

```rust
#[derive(gpu::Payload)]
struct Radiance { color: Vec3, depth: u32 }

#[gpu::raygen]
fn camera(t: Thread, scene: &Tlas, out: Out<Rgba>) { /* trace::<Radiance>(scene, ray) */ }

#[gpu::closest_hit]
fn lit(hit: &Hit, p: &mut Radiance, mat: &Material) { /* ... */ }

#[gpu::miss]
fn sky(p: &mut Radiance) { p.color = SKY.rgb(); }

let pipeline = gpu::Pipeline::new(&dev)
    .raygen(camera)
    .hit_group(HitGroup::closest(lit).with_record(&materials)) // binding table entry per material
    .miss(sky)
    .max_recursion(2)
    .build()?;
```

- `trace::<P>` compiles only if every hit and miss shader in the pipeline takes `&mut P`.
- blaze generates the binding table from `with_record`'s type, so the table and the shader
  parameters can't disagree.
- `max_recursion` is checked at build time against the call graph. A shader that traces deeper is
  a typed error naming it.
- On CUDA it maps one to one to OptiX. On Metal it maps to intersection function tables plus a
  compute kernel, which costs some hardware scheduling the other APIs have.

## Acceleration structures with lifetimes

```rust
let blas = dev.build_blas(&BlasDesc::triangles(&verts, &indices))?;  // Blas<'verts>
let tlas = dev.build_tlas(&[Instance::new(&blas, xform, 0)])?;      // Tlas<'blas>
```

- A `Blas` borrows its vertex and index buffers while the build reads them. After a compacting
  build the borrow ends, because the structure no longer needs them.
- A `Tlas<'blas>` borrows every `Blas` it references, so dropping a BLAS the TLAS still points at
  doesn't compile. In Vulkan that's a GPU fault or silently wrong hits.
- Refits and rebuilds take `&mut Tlas`, so a refit can't race a launch reading the same TLAS. The
  launch's borrow, or ownership passing as in [plan.md](plan.md#memory-safety-by-construction),
  holds it.
- An instance's custom index and mask are typed fields, never a packed `u32` someone has to
  decode by hand.

## Phases

1. Inline `RayQuery` on Vulkan SPIR-V and on the CPU (Embree), triangles only.
2. Metal through SPIRV-Cross, or blaze's own MSL for the query.
3. Procedural candidates (`commit_at`), and acceleration structures with lifetimes on every backend.
4. The pipeline model with typed payloads and generated binding tables, on Vulkan and OptiX.
5. Inline query on CUDA through the loop-to-OptiX split.

## Open questions

- Does LLVM 23's SPIR-V backend emit `OpRayQuery*`, or does blaze need its own SPIR-V emitter for
  these instructions?
- How complete is SPIRV-Cross's ray query to MSL translation, and does it cover procedural hits?
- Should `Device::cpu()` link Embree (C++, Apache-2.0, x86 and arm64) or use a BVH written in
  Rust? Embree is faster today, but a Rust BVH runs under Miri and builds with blaze alone.
- Motion blur and curves: Vulkan, OptiX and Metal all have them, with different shapes. Leave them
  out until someone needs them.

## Prior art

- rust-gpu's `spirv-std` has `RayQuery` and ray tracing pipeline entry points, untyped payloads.
- Slang supports ray tracing stages and `RayQuery` across DXR, Vulkan and Metal.
- OptiX 8 and 9: the pipeline model and binding tables, with no inline query.
- Embree 4: CPU ray queries, and SYCL for Intel GPUs.
