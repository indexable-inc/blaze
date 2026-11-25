//! `blaze-pbr`: Reusable `#![no_std]` sans-IO physically-based rendering & CC0 material library.
//!
//! Designed to be imported both by CAD (`blaze-cad-picture`, `blaze-cad`, `blaze-cad-cloth`) and
//! by external game engines, Flecs worlds, and renderers built on blaze:
//!
//! | module | what |
//! |---|---|
//! | [`brdf`] | Cook-Torrance GGX microfacet BRDF (anisotropy, height-correlated Smith G2, multi-scattering boost, clearcoat, Charlie sheen, dielectric IOR) |
//! | [`tonemap`] | AgX, AgX Punchy, and ACES Fitted filmic tone mapping + deterministic sRGB conversion |
//! | [`env`] | Procedural HDRI environments (`StudioSoftbox`, `GoldenHour`, `OvercastSky`, `HangarBay`) and deterministic roughness-filtered IBL + transmission |
//! | [`texture`] | Bilinear, stochastic hex-tile anti-repetition, and world-space triplanar sampling, tangent-space normal decoding, RNM normal blending, and deterministic 3D fBm noise |
//! | [`smart`] | Context-aware smart-material layering driven by `[AO, Curvature, Cavity, Height]` masks (edge wear, crevice grime, upward dust, macro breakup) |
//! | [`cc0`] | CC0 PBR texture pack descriptors (`Cc0PackView`) and seamless procedural PBR texture synthesis (`TimberPlanks`, `SlateShingles`, `AshlarStone`, `HullPlating`, `BrushedMetal`) |

#![no_std]
#![feature(register_tool, custom_inner_attributes, core_float_math)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod bloom;
pub mod brdf;
pub mod cc0;
pub mod denoise;
pub mod env;
pub mod smart;
pub mod texture;
pub mod tonemap;

pub use bloom::{PostProcessOpts, post_process_hdr};
pub use brdf::{
    SurfacePbr, charlie_d, dielectric_f0, eval_direct_brdf, fresnel_schlick, fresnel_schlick_roughness, ggx_d, ggx_d_aniso,
    multi_scatter_boost, sample_ggx_vndf, smith_v_ggx, specular_ao,
};
pub use cc0::{Cc0PackView, ProceduralKind, SynthesizedPack, synthesize_cc0_pack};
pub use denoise::{DenoiseOpts, GBufferPixel, denoise_atrous};
pub use env::{DominantLight, EnvironmentPreset, dominant_light, eval_ibl, eval_ibl_with_irradiance, sample_environment};
pub use smart::{SmartMaterial, SurfaceMasks, apply_smart_material};
pub use texture::{
    OwnedTexture, TextureView, decode_tangent_normal, fbm3, hash2, hash3, noise3, perturb_normal, rnm_blend, sample_bilinear,
    sample_hex_tile, sample_triplanar,
};
pub use tonemap::{ToneMap, apply_tonemap, linear_to_srgb, quantize_u8, srgb_to_linear, srgb_to_linear_u8};

#[cfg(test)]
mod tests {
    use blaze_math::Vec3;
    use blaze_own::testing::heap;

    use super::*;

    #[test]
    fn ggx_brdf_energy_and_metallic_fresnel_behave_physically() {
        let n = Vec3::new(0.0, 0.0, 1.0);
        let t = Vec3::new(1.0, 0.0, 0.0);
        let b = Vec3::new(0.0, 1.0, 0.0);
        let v = Vec3::new(0.0, -0.5, 0.8660254).normalized().unwrap();
        let l = Vec3::new(0.0, 0.5, 0.8660254).normalized().unwrap(); // exact mirror reflection

        let metal = SurfacePbr { base_colour: [0.95, 0.72, 0.35], metallic: 1.0, roughness: 0.15, ..SurfacePbr::DEFAULT };
        let matte = SurfacePbr { base_colour: [0.95, 0.72, 0.35], metallic: 0.0, roughness: 0.85, ..SurfacePbr::DEFAULT };
        let r_metal = eval_direct_brdf(&metal, n, t, b, v, l);
        let r_matte = eval_direct_brdf(&matte, n, t, b, v, l);

        assert!(r_metal[0] > r_matte[0] * 3.0, "smooth gold mirror highlight must vastly exceed matte: {r_metal:?} vs {r_matte:?}");
        assert!(r_metal[0] > r_metal[2] * 2.0, "gold specular must preserve metallic tint: {r_metal:?}");
        assert!(r_metal.iter().all(|x| x.is_finite() && *x >= 0.0));
    }

    #[test]
    fn agx_and_aces_tonemap_compress_hdr_monotonically_without_nan() {
        for mode in [ToneMap::AgX, ToneMap::AgXPunchy, ToneMap::Aces] {
            let low = apply_tonemap([0.2, 0.2, 0.2], 1.0, mode);
            let mid = apply_tonemap([1.0, 1.0, 1.0], 1.0, mode);
            let high = apply_tonemap([12.0, 12.0, 12.0], 1.0, mode);
            assert!(low[0] < mid[0] && mid[0] < high[0], "{mode:?} must be strictly monotonic: {low:?} {mid:?} {high:?}");
            assert!(high[0] <= 1.0 && high[0] > 0.75, "{mode:?} must compress HDR smoothly into (0.75, 1.0]: {high:?}");
        }
    }

    #[test]
    fn smart_material_wears_convex_edges_and_darkens_concave_crevices() {
        let base = SurfacePbr { base_colour: [0.18, 0.26, 0.34], metallic: 0.1, roughness: 0.45, ..SurfacePbr::DEFAULT };
        let up = Vec3::new(0.0, 0.0, 1.0);
        let p = Vec3::new(0.5, 0.25, 0.1);
        let n = Vec3::new(0.0, 0.0, 1.0);

        let convex = apply_smart_material(
            base,
            SurfaceMasks { ao: 1.0, curvature: 0.90, cavity: 1.0, height: 0.5 },
            p,
            n,
            up,
            &SmartMaterial::PAINTED_METAL,
            1.0,
        );
        let concave = apply_smart_material(
            base,
            SurfaceMasks { ao: 0.25, curvature: 0.10, cavity: 0.15, height: 0.5 },
            p,
            n,
            up,
            &SmartMaterial::PAINTED_METAL,
            1.0,
        );
        assert!(convex.metallic > base.metallic + 0.25, "convex edge must expose bare metal: {}", convex.metallic);
        assert!(convex.base_colour[0] > concave.base_colour[0] * 2.0, "convex wear must be brighter than crevice grime");
        assert!(concave.roughness > convex.roughness + 0.25, "crevice grime must be rougher than burnished edge");
    }

    #[test]
    fn synthesize_cc0_pack_is_deterministic_and_non_trivial() {
        let h = heap().alloc();
        let a = synthesize_cc0_pack(
            ProceduralKind::TimberPlanks { base_rgb: [0.36, 0.20, 0.10], planks: 6 },
            64,
            h,
            h,
        )
        .expect("synth a");
        let b = synthesize_cc0_pack(
            ProceduralKind::TimberPlanks { base_rgb: [0.36, 0.20, 0.10], planks: 6 },
            64,
            h,
            h,
        )
        .expect("synth b");
        assert_eq!(&*a.albedo.pixels, &*b.albedo.pixels);
        assert_eq!(&*a.normal.pixels, &*b.normal.pixels);
        assert_eq!(&*a.orm.pixels, &*b.orm.pixels);
        let non_flat = a.normal.pixels.chunks_exact(4).filter(|p| p[0] != 128 || p[1] != 128).count();
        assert!(non_flat > 500, "synthesized wood normal map must have rich grain/plank relief: {non_flat}");

        // Hex tiling returns valid finite normalized colors.
        let s = sample_hex_tile(&a.albedo.view(), [3.7, 2.4], 0.5);
        assert!(s.iter().all(|c| c.is_finite() && (0.0..=1.0).contains(c)));
    }

    #[test]
    fn hdr_post_process_bloom_creates_deterministic_emissive_halo() {
        let h = heap().alloc();
        let w = 32u32;
        let ht = 32u32;
        let mut hdr = blaze_own::Vec::try_with_capacity_in((w * ht) as usize, h).unwrap();
        for y in 0..ht {
            for x in 0..w {
                let c = if (14..=17).contains(&x) && (14..=17).contains(&y) {
                    [0.8, 10.0, 14.0]
                } else {
                    [0.01, 0.01, 0.01]
                };
                hdr.try_push(c).unwrap();
            }
        }
        let opts_on = PostProcessOpts {
            bloom_threshold: 1.0,
            bloom_strength: 0.25,
            vignette: 0.0,
            ..PostProcessOpts::DEFAULT
        };
        let opts_off = PostProcessOpts {
            bloom_strength: 0.0,
            vignette: 0.0,
            ..opts_on
        };
        let out_bloom = post_process_hdr(&hdr, w, ht, opts_on, h, h).unwrap();
        let out_bloom2 = post_process_hdr(&hdr, w, ht, opts_on, h, h).unwrap();
        let out_none = post_process_hdr(&hdr, w, ht, opts_off, h, h).unwrap();

        assert_eq!(&*out_bloom, &*out_bloom2, "HDR bloom must be bit-identical across runs");
        // Pixel (19, 15) is 2px outside the emissive block: bloom must brighten its cyan channels!
        let idx = ((15 * w + 19) * 4) as usize;
        assert!(
            out_bloom[idx + 2] > out_none[idx + 2] + 15,
            "bloom must spill cyan halo onto neighboring dark pixels: with={} without={}",
            out_bloom[idx + 2],
            out_none[idx + 2]
        );
    }

    #[test]
    fn vndf_ggx_samples_tight_lobe_when_smooth_and_broad_when_rough() {
        let n = Vec3::new(0.0, 0.0, 1.0);
        let t = Vec3::new(1.0, 0.0, 0.0);
        let b = Vec3::new(0.0, 1.0, 0.0);
        let v = Vec3::new(0.0, -0.6, 0.8).normalized().unwrap();
        let mirror = Vec3::new(0.0, 0.6, 0.8).normalized().unwrap();

        let mut dot_smooth = 0.0;
        let mut dot_rough = 0.0;
        for i in 0..16 {
            let u1 = (i as f64 + 0.5) / 16.0;
            let u2 = ((i as f64) * 0.6180339887).fract();
            let ls = sample_ggx_vndf(v, n, t, b, 0.05, 0.0, u1, u2);
            let lr = sample_ggx_vndf(v, n, t, b, 0.65, 0.0, u1, u2);
            dot_smooth += ls.dot(mirror);
            dot_rough += lr.dot(mirror);
        }
        assert!(dot_smooth / 16.0 > 0.98, "smooth GGX VNDF must hug mirror vector: {}", dot_smooth / 16.0);
        assert!(dot_rough / 16.0 < dot_smooth / 16.0 - 0.15, "rough GGX VNDF must spread wider than smooth");
    }

    #[test]
    fn atrous_denoiser_smooths_flat_noise_while_preserving_sharp_albedo_and_depth_edges() {
        let h = heap().alloc();
        let w = 24u32;
        let ht = 24u32;
        let mut hdr = blaze_own::Vec::try_with_capacity_in((w * ht) as usize, h).unwrap();
        let mut gbuf = blaze_own::Vec::try_with_capacity_in((w * ht) as usize, h).unwrap();
        for y in 0..ht {
            for x in 0..w {
                let right_half = x >= 12;
                let noise = if (x + y) % 2 == 0 { 0.12 } else { -0.12 };
                let base = if right_half { 0.80 } else { 0.20 };
                hdr.try_push([base + noise, base + noise, base + noise]).unwrap();
                gbuf.try_push(GBufferPixel {
                    albedo: if right_half { [0.8, 0.8, 0.8] } else { [0.2, 0.2, 0.2] },
                    normal: [0.0, 0.0, 1.0],
                    depth: if right_half { 2.0 } else { 5.0 },
                    preserve: 0.0,
                })
                .unwrap();
            }
        }
        let mut hdr_copy = blaze_own::Vec::try_with_capacity_in((w * ht) as usize, h).unwrap();
        for &c in hdr.iter() {
            hdr_copy.try_push(c).unwrap();
        }
        denoise_atrous(&mut hdr, &gbuf, w, ht, DenoiseOpts::DEFAULT, h).unwrap();
        denoise_atrous(&mut hdr_copy, &gbuf, w, ht, DenoiseOpts::DEFAULT, h).unwrap();
        assert_eq!(&*hdr, &*hdr_copy, "denoiser must be deterministic");

        // Flat interior checkerboard noise must be drastically reduced.
        let p_a = hdr[(10 * w + 5) as usize][0];
        let p_b = hdr[(10 * w + 6) as usize][0];
        assert!((p_a - p_b).abs() < 0.05, "flat interior noise must smooth out: {p_a} vs {p_b}");

        // Sharp depth + albedo edge at x=11 vs x=12 must stay razor sharp!
        let left_edge = hdr[(10 * w + 11) as usize][0];
        let right_edge = hdr[(10 * w + 12) as usize][0];
        assert!(right_edge - left_edge > 0.45, "sharp G-buffer edge must not bleed: left={left_edge} right={right_edge}");
    }
}
