---
status: current
summary: Reusable blaze-pbr library, semantic UVs + tangents, analytic SDF attribute & normal-map baking, full glTF 2.0 PBR + Flecs v4.2 prefab emission, and photoreal GGX + 1-bounce GI + NEE + Bloom previewer across blaze-pbr, blaze-cad-mesh, blaze-cad-out, blaze-cad-picture, and blaze-cad.
date: 2026-11-24
---
# CAD PBR Pipeline, Reusable `blaze-pbr`, SDF Baking, Photoreal GI Previewer, and Flecs v4.2 Prefabs

Extends D183 (`docs/v2/plan/design/end-state.md`) so CAD models export directly into beyond-AAA PBR
workflows (CC0 PBR packs + SDF-driven wear/AO/cavity smart materials + analytic normal maps) and
Flecs v4.2 ECS worlds without leaving `#![no_std]` sans-IO deterministic Rust. All renderer-agnostic
PBR math, environments, texture sampling, smart materials, procedural CC0 synthesis, and HDR optical
bloom live in the standalone reusable library crate `crates/blaze-pbr`.

## Implemented architecture (and what it beats)

1. **Standalone reusable PBR library (`crates/blaze-pbr`).**
   - `#![no_std]`, `#![blaze::sans_io]`, `#![forbid(unsafe_code)]`, zero third-party deps, usable
     directly by CAD previewers, game engines, and Flecs worlds alike.
   - Modules: `brdf` (Cook-Torrance GGX, Heitz 2018 VNDF rough GGX importance sampling `sample_ggx_vndf`,
     anisotropy, height-correlated Smith G2, multi-scattering, clearcoat, Charlie sheen, dielectric IOR),
     `tonemap` (AgX, AgX Punchy, ACES), `env` (photographic procedural HDRI environments: high-contrast
     darkroom `StudioSoftbox`, alpine `GoldenHour` with backlit clouds + snow-dusted peaks + forested
     foothills, `OvercastSky`, architectural steel `HangarBay` with clerestory windows + twin LED banks
     + sodium bay lamps, plus `eval_ibl` / `eval_ibl_with_irradiance`), `texture` (bilinear, hex-tile
     anti-repetition, triplanar, tangent-space normal decode, RNM normal blending, 3D fBm), `smart`
     (SDF-mask driven edge wear, crevice grime, upward dust, macro breakup), `cc0` (12 seamless procedural
     CC0 PBR pack generators: `TimberPlanks`, `MilledWood`, `SlateShingles`, `AshlarStone`, `HullPlating`,
     `BrushedMetal`, `CarbonWeave`, `WroughtIron`, `AgedCopper`, `TreeBark`, `PineFoliage`,
     `WeatheredRock`), `denoise` (edge-preserving G-buffer À-Trous wavelet denoiser with exact B3-spline
     weights, `[1, 1, 2, 4, 8]` stride schedule, depth/normal/albedo/luma edge-stopping, and 3×3 firefly
     clamping), and `bloom` (deterministic half-res dual-radius separable HDR optical bloom + vignette +
     AgX/ACES post-processing).

2. **Post-shade semantic metric UVs & orthonormal tangents (`blaze-cad-mesh::uv`).**
   - `project_uvs` supports `BoxMetric`, shelf-packed uniform-density `BoxAtlas` (auto-rotates tall
     charts and bisects optimal uniform texel density with exact pixel gutters), `Planar`, and
     `Cylindrical`, splitting chart seams cleanly. `compute_tangents` builds MikkTSpace-compatible
     Gram-Schmidt orthonormal `[f32; 4]` glTF tangents (`xyz` + handedness `w`).
   - *Losing alternative:* preserving UVs through QEM decimation before crease shading introduces
     seam distortion and explodes edge-collapse complexity; projecting cleanly after decimation and
     crease shading guarantees exact metric scale and zero seam stretch.

3. **Analytic SDF surface intelligence & normal-map baking (`blaze-cad-mesh::bake`).**
   - `bake_sdf_surface` evaluates the exact assembly/part `Tape` at each vertex to pack
     `[AO, Curvature, Cavity, HeightOrThickness]` into `COLOR_0` (`[u8; 4]`), enabling
     context-aware wear, contact darkening, and crevice grime with zero ray-cage bake artifacts.
   - `bake_sdf_normal_map` and `bake_sdf_mask_map` rasterize UV triangles against the exact SDF
     gradient (`eval_grad`) into tangent-space RGBA8 normal maps and RGBA8 surface mask maps with
     deterministic 8-neighbor gutter dilation.
   - *Losing alternative:* cage ray-casting against a dense high-poly mesh costs 10x memory and
     suffers corner misses; the implicit SDF gradient is exact everywhere.

4. **Full glTF 2.0 PBR + attributes + Flecs v4.2 prefab emitter (`blaze-cad-out`).**
   - `glb.rs` emits `TEXCOORD_0`, `TANGENT`, `COLOR_0`, embedded `images` (PNG/JPEG or auto-encoded
     RGBA8), `samplers`, `textures`, texture slots (`baseColorTexture`, `metallicRoughnessTexture`,
     `normalTexture`, `occlusionTexture`, `emissiveTexture`, `KHR_texture_transform`), and advanced
     material extensions (`KHR_materials_anisotropy`, `KHR_materials_sheen`,
     `KHR_materials_transmission`, `KHR_materials_ior`, `KHR_materials_volume`), `extras.cc0_pack`,
     and `extras.sockets`. Byte-identical to the minimal golden GLB when unused.
   - `flecs.rs` emits deterministic Flecs v4.2 scripts (`@tree ChildOf prefab`, `(OnInstantiate, Copy)`
     for per-instance overrides like `Transform`, `MaterialWear`, `Joint`; `(OnInstantiate, Inherit)`
     for shared geometry, colliders, mass, LODs, and named sockets; `(cad.UsesMaterial, ...)` edges).

5. **Photoreal GGX + VNDF + 1-bounce BVH GI + NEE + DoF + À-Trous Denoiser previewer (`blaze-cad-picture`).**
   - `PreparedMeshScene` + `Copy + Send + Sync` `PreparedMeshSceneRef` + `PbrRenderParams` allow
     multi-core hosts to render horizontal bands of a 960×640+ 9–16 SPP HDR frame in parallel with
     zero BVH rebuilds and `#![forbid(unsafe_code)]`, while keeping `blaze-cad-picture` 100% `#![no_std]`
     sans-IO.
   - `Shading::Pbr(PbrOpts)` accumulates a full linear HDR + G-buffer supersampled (`1..=4`, up to 16 SPP)
     framebuffer over the BVH mesh renderer with `blaze-pbr` GGX + HDRI IBL, Heitz 2018 VNDF rough
     reflections (contact-hardening on metals & floors), physical thin-lens Depth of Field bokeh
     (`aperture_radius_mm`), outdoor aerial perspective haze (`atmosphere_haze`), Cranley-Patterson rotated
     cosine-weighted hemisphere sky visibility & 1-bounce diffuse color bleeding (with clear-glass sky
     transmission into interiors), deterministic emissive-mesh clustering with glass-transmitting
     Next-Event Estimation (NEE) on both primary and transmitted/reflected hits, rotated Vogel-disk soft
     shadows, closed-solid glass exit-face pass-through refraction, environment-matched PBR ground planes,
     G-buffer À-Trous denoising, and `post_process_hdr` optical bloom + vignette + AgX Punchy.

6. **End-to-end game-asset integration & 3-asset photoreal showcase (`blaze-cad`).**
   - `Part::uv_mode` / `Model::set_uv_mode`, `Model::assembly_tape`, `PartSocket`, `PbrMeshOpts`,
     `GameAsset::prepare_pbr_scene`, `GameAsset::export_flecs_prefab`, and `GameAsset::preview_pbr` wire
     the whole pipeline together.
   - `crates/blaze-cad/examples/pbr_hero.rs` builds and renders three complete multi-part photoreal hero
     assets at 960×640 9-SPP (8 threads) to `.glb`, `.flecs`, and denoised HDR PNGs:
     1. **Alpine Cedar & Slate Mountain Cabin** (20 parts, furnished 3D Edison-lit interior behind
        reflective glass, aged copper gutter/ridge, wrought-iron hardware, mossy `WeatheredRock` outcrops,
        stacked firewood rack, cedar porch bench).
     2. **Stealth Orbital Recon Interceptor** (5-station lofted gunmetal titanium fuselage, carbon-slate
        wings, pitot probe, curved titanium canopy bow over carbon-weave cockpit & HUD, wheeled landing
        gear, brushed titanium turbine nozzles with cyan plasma rings).
     3. **Precision CNC 50mm f/1.4 Optical Camera Lens** (28-flute CNC focus knurling, 24-flute aperture
        ring, brushed titanium bayonet & scale collar with cylindrical UVs, brass accent ring, Leica ruby
        index dot, matte-black optical light-trap & iris diaphragm, emerald & sapphire AR-coated convex
        optical glass elements).

## Cost budget & determinism

- Zero `Global` allocations, zero third-party crates, `#![no_std]` + `#![blaze::sans_io]`.
- UV projection and tangent computation: $O(V + T)$ linear time, deterministic vertex/chart order.
- Vertex SDF baking: $O(V \cdot (\text{ao\_steps} + 6))$ tape evaluations; normal-map baking bounded
  by `width * height` pixels.
- Every output (`.glb`, `.flecs`, `.png`, vertex columns) is byte-for-byte deterministic.

## Verification

- Unit & property tests in each crate covering UV metric scale, tangent orthonormality/handedness,
  SDF AO/curvature contrast (convex corner > flat > concave corner), normal-map encoding & dilation,
  glTF 2.0 JSON/BIN structural validation for all new attributes/textures/extensions, Flecs v4.2
  syntax & hierarchy checks, GGX/AgX energy bounds & wear response, and planted-bug controls.
- Real-run end-to-end verification on `cabin` and a PBR showcase exporting `.glb`, `.flecs`, baked
  normal maps, and photoreal GGX+IBL beauty renders.
