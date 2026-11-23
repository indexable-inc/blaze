//! Deterministic surface shading, SDF ambient occlusion, soft SDF shadows, movable studio strip
//! lights, render modes (`Shaded`, `Normals`, `Depth`, `Matcap`, `Zebra`), and gamma-aware
//! supersample resolve.

#![blaze::sans_io]

use blaze_cad_field::{Scratch, Tape, eval_point};
use blaze_math::{Aabb3, Vec3};

use crate::{Camera, LEAST_STEP, PictureOpts, PictureStats, RenderMode, Shading, StripLight, StudioOpts, ZebraOpts, slab};

/// Deterministic 2×2 rotated-grid supersampling (RGSS) offsets within `[0, 1]²`.
/// Every coordinate is an exact dyadic fraction (`k / 8`).
pub(crate) const RGSS_2X2: [(f64, f64); 4] = [
    (0.375, 0.125),
    (0.875, 0.375),
    (0.125, 0.625),
    (0.625, 0.875),
];

/// Precomputed camera-frame lights, albedo, and depth range shared by all pixels of a render.
#[derive(Clone, Copy)]
pub(crate) struct ShadeContext {
    pub classic_light: Vec3,
    pub key_dir: Vec3,
    pub fill_dir: Vec3,
    pub rim_dir: Vec3,
    pub scene_diag: f64,
    pub z_near: f64,
    pub z_far: f64,
    pub base_linear: [f64; 3],
    pub base_norm: [f64; 3],
    pub bg_linear: [f64; 4],
    pub bg_norm: [f64; 4],
    pub linear_blend: bool,
    pub bounds: Aabb3,
}

impl ShadeContext {
    pub(crate) fn new(cam: &Camera, bounds: Aabb3, o: &PictureOpts<'_, '_>) -> ShadeContext {
        let classic_light = (cam.up * 0.7 - cam.right * 0.4 - cam.forward * 0.6).normalized().unwrap_or(cam.up);
        let key_dir = (cam.up * 0.72 - cam.right * 0.45 - cam.forward * 0.52).normalized().unwrap_or(cam.up);
        let fill_dir = (cam.up * 0.20 + cam.right * 0.85 - cam.forward * 0.48).normalized().unwrap_or(cam.right);
        let rim_dir = (cam.up * 0.55 + cam.right * 0.35 + cam.forward * 0.75).normalized().unwrap_or(cam.forward);

        let (scene_diag, z_near, z_far) = if bounds.is_empty() {
            (1.0, 0.0, 1.0)
        } else {
            let diag = (bounds.hi - bounds.lo).length().max(LEAST_STEP * 1e3);
            let mut zn = f64::INFINITY;
            let mut zf = f64::NEG_INFINITY;
            for ix in 0..2 {
                let x = if ix == 0 { bounds.lo.x } else { bounds.hi.x };
                for iy in 0..2 {
                    let y = if iy == 0 { bounds.lo.y } else { bounds.hi.y };
                    for iz in 0..2 {
                        let z = if iz == 0 { bounds.lo.z } else { bounds.hi.z };
                        let proj = (Vec3::new(x, y, z) - cam.eye).dot(cam.forward);
                        zn = zn.min(proj);
                        zf = zf.max(proj);
                    }
                }
            }
            let zn = zn.max(0.0);
            let zf = zf.max(zn + LEAST_STEP);
            (diag, zn, zf)
        };

        let base_norm = [
            f64::from(o.colour[0]) / 255.0,
            f64::from(o.colour[1]) / 255.0,
            f64::from(o.colour[2]) / 255.0,
        ];
        let base_linear = [
            srgb_to_linear(o.colour[0]),
            srgb_to_linear(o.colour[1]),
            srgb_to_linear(o.colour[2]),
        ];
        let bg_norm = [
            f64::from(o.background[0]) / 255.0,
            f64::from(o.background[1]) / 255.0,
            f64::from(o.background[2]) / 255.0,
            f64::from(o.background[3]) / 255.0,
        ];
        let bg_linear = [
            srgb_to_linear(o.background[0]),
            srgb_to_linear(o.background[1]),
            srgb_to_linear(o.background[2]),
            bg_norm[3],
        ];
        let linear_blend = matches!((o.mode, o.shading), (RenderMode::Shaded, Shading::Studio(s)) if s.srgb);

        ShadeContext {
            classic_light,
            key_dir,
            fill_dir,
            rim_dir,
            scene_diag,
            z_near,
            z_far,
            base_linear,
            base_norm,
            bg_linear,
            bg_norm,
            linear_blend,
            bounds,
        }
    }

    #[inline]
    pub(crate) fn miss_sample(&self) -> [f64; 4] {
        if self.linear_blend { self.bg_linear } else { self.bg_norm }
    }
}

/// Deterministic sRGB byte to linear `[0, 1]` using `blaze_math::pow`.
#[inline]
pub(crate) fn srgb_to_linear(u: u8) -> f64 {
    let c = f64::from(u) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        blaze_math::pow((c + 0.055) / 1.055, 2.4)
    }
}

/// Deterministic linear `[0, 1]` to sRGB `[0, 1]` using `blaze_math::pow`.
#[inline]
pub(crate) fn linear_to_srgb(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * blaze_math::pow(c, 1.0 / 2.4) - 0.055
    }
}

/// Quantize a normalized `[0, 1]` channel to `u8`, rounded to nearest-even.
#[inline]
pub(crate) fn quantize(v: f64) -> u8 {
    let scaled = core::f64::math::round_ties_even(v.clamp(0.0, 1.0) * 255.0);
    scaled.max(0.0).min(255.0) as u8
}

/// One channel shaded: `c × s`, rounded to nearest-even, within `0..=255`.
#[inline]
pub(crate) fn channel(c: u8, s: f64) -> u8 {
    let v = core::f64::math::round_ties_even(f64::from(c) * s);
    v.max(0.0).min(255.0) as u8
}

/// Resolve an accumulated RGBA sample sum back to RGBA8 bytes.
#[inline]
pub(crate) fn resolve_samples(sum: [f64; 4], inv_count: f64, linear_blend: bool) -> [u8; 4] {
    let r = sum[0] * inv_count;
    let g = sum[1] * inv_count;
    let b = sum[2] * inv_count;
    let a = sum[3] * inv_count;
    if linear_blend {
        [quantize(linear_to_srgb(r)), quantize(linear_to_srgb(g)), quantize(linear_to_srgb(b)), quantize(a)]
    } else {
        [quantize(r), quantize(g), quantize(b), quantize(a)]
    }
}

/// Multi-scale deterministic SDF ambient occlusion along `n`, evaluated against `full_tape`.
pub(crate) fn compute_ao(
    full_tape: &Tape<'_>,
    p: Vec3,
    n: Vec3,
    eps: f64,
    scene_diag: f64,
    steps: u8,
    strength: f64,
    sc: &mut Scratch<'_>,
    stats: &mut PictureStats,
) -> f64 {
    let strength = strength.clamp(0.0, 1.0);
    if steps == 0 || !(strength > 0.0) {
        return 1.0;
    }
    let max_dist = (scene_diag * 0.08).max(eps * 8.0);
    let mut occ = 0.0;
    let mut weight = 0.5;
    let mut weight_sum = 0.0;
    let n_steps = f64::from(steps);
    for i in 1..=steps {
        let frac = f64::from(i) / n_steps;
        let h = (max_dist * frac).max(eps * 2.0 * f64::from(i));
        stats.points += 1;
        let d = eval_point(full_tape, p + n * h, sc);
        let deficit = ((h - d) / h).clamp(0.0, 1.0);
        occ += weight * deficit;
        weight_sum += weight;
        weight *= 0.5;
    }
    let ao_raw = if weight_sum > 0.0 { 1.0 - (1.5 * occ / weight_sum).clamp(0.0, 1.0) } else { 1.0 };
    (1.0 - strength) + strength * ao_raw.clamp(0.0, 1.0)
}

/// Deterministic soft SDF shadow marching toward `light_dir` (Aaltonen / IQ improved penumbra),
/// evaluated against `full_tape`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_shadow(
    full_tape: &Tape<'_>,
    bounds: &Aabb3,
    p: Vec3,
    n: Vec3,
    light_dir: Vec3,
    eps: f64,
    scene_diag: f64,
    max_steps: u8,
    softness: f64,
    strength: f64,
    sc: &mut Scratch<'_>,
    stats: &mut PictureStats,
) -> f64 {
    let strength = strength.clamp(0.0, 1.0);
    if max_steps == 0 || !(strength > 0.0) || !(softness > 0.0) {
        return 1.0;
    }
    // Back-facing surfaces receive zero key diffuse anyway; skip shadow marching.
    if n.dot(light_dir) <= 0.0 {
        return 1.0 - strength;
    }
    let bias = (eps * 3.0).max(scene_diag * 1e-4);
    let p_start = p + n * bias;
    let Some((_, t_max)) = slab(bounds, p_start, light_dir) else {
        return 1.0;
    };
    let min_step = (scene_diag * 1e-4).max(eps);
    let mut t_s = (eps * 2.0).max(scene_diag * 1e-4);
    if t_s >= t_max {
        return 1.0;
    }
    let mut res = 1.0f64;
    let mut ph = 1e20f64;
    for _ in 0..max_steps {
        if t_s >= t_max {
            break;
        }
        stats.points += 1;
        let h = eval_point(full_tape, p_start + light_dir * t_s, sc);
        if h <= 0.0 {
            res = 0.0;
            break;
        }
        let y = if ph < 1e19 { (h * h) / (2.0 * ph) } else { 0.0 };
        let d_sq = (h * h - y * y).max(0.0);
        let d = blaze_math::sqrt(d_sq);
        let denom = (t_s - y).max(min_step);
        res = res.min(softness * d / denom);
        if res <= 1e-3 {
            res = 0.0;
            break;
        }
        ph = h;
        t_s += h.max(min_step);
    }
    (1.0 - strength) + strength * res.clamp(0.0, 1.0)
}

/// Shade a hit point into an RGBA `[0, 1]` sample (in linear space if `ctx.linear_blend`, else display space).
#[allow(clippy::too_many_arguments)]
pub(crate) fn shade_hit_sample(
    full_tape: &Tape<'_>,
    cam: &Camera,
    ctx: &ShadeContext,
    o: &PictureOpts<'_, '_>,
    p: Vec3,
    dir: Vec3,
    n: Vec3,
    eps: f64,
    sc: &mut Scratch<'_>,
    stats: &mut PictureStats,
) -> [f64; 4] {
    match o.mode {
        RenderMode::Normals => [
            (n.x * 0.5 + 0.5).clamp(0.0, 1.0),
            (n.y * 0.5 + 0.5).clamp(0.0, 1.0),
            (n.z * 0.5 + 0.5).clamp(0.0, 1.0),
            1.0,
        ],
        RenderMode::Depth => {
            let proj = (p - cam.eye).dot(cam.forward);
            let span = (ctx.z_far - ctx.z_near).max(LEAST_STEP);
            let u = ((proj - ctx.z_near) / span).clamp(0.0, 1.0);
            let v = 1.0 - u;
            [v, v, v, 1.0]
        }
        RenderMode::Matcap => shade_matcap(cam, ctx, n),
        RenderMode::Zebra(zebra) => shade_zebra(cam, dir, n, &zebra),
        RenderMode::Shaded => match o.shading {
            Shading::Classic => {
                let s = 0.22 + 0.78 * n.dot(ctx.classic_light).max(0.0);
                [
                    (ctx.base_norm[0] * s).clamp(0.0, 1.0),
                    (ctx.base_norm[1] * s).clamp(0.0, 1.0),
                    (ctx.base_norm[2] * s).clamp(0.0, 1.0),
                    1.0,
                ]
            }
            Shading::Studio(studio) => shade_studio(full_tape, cam, ctx, &studio, p, dir, n, eps, sc, stats),
        },
    }
}

/// Fast path for single-sample classic shading matching the original `channel(c, s)` rounding bit-for-bit.
#[inline]
pub(crate) fn shade_classic_u8(ctx: &ShadeContext, colour: [u8; 3], n: Vec3) -> [u8; 4] {
    let s = 0.22 + 0.78 * n.dot(ctx.classic_light).max(0.0);
    [channel(colour[0], s), channel(colour[1], s), channel(colour[2], s), 255]
}

/// Studio 3-point CAD shading with hemisphere ambient, Blinn-Phong specular, Fresnel rim,
/// SDF ambient occlusion, and soft SDF shadows.
#[allow(clippy::too_many_arguments)]
fn shade_studio(
    full_tape: &Tape<'_>,
    cam: &Camera,
    ctx: &ShadeContext,
    studio: &StudioOpts,
    p: Vec3,
    dir: Vec3,
    n: Vec3,
    eps: f64,
    sc: &mut Scratch<'_>,
    stats: &mut PictureStats,
) -> [f64; 4] {
    let v = dir * -1.0;
    let ao = compute_ao(full_tape, p, n, eps, ctx.scene_diag, studio.ao_steps, studio.ao_strength, sc, stats);
    let shadow = compute_shadow(
        full_tape,
        &ctx.bounds,
        p,
        n,
        ctx.key_dir,
        eps,
        ctx.scene_diag,
        studio.shadow_steps,
        studio.shadow_softness,
        studio.shadow_strength,
        sc,
        stats,
    );

    let albedo = if studio.srgb { ctx.base_linear } else { ctx.base_norm };

    // Key light (warm studio softbox, upper-left front; mild area-light crevice occlusion)
    let key_ao = 0.65 + 0.35 * ao;
    let ndotl_key = n.dot(ctx.key_dir).max(0.0) * studio.key * shadow * key_ao;
    let key_tint = [1.03, 1.00, 0.96];

    // Fill light (cool opposite side)
    let ndotl_fill = n.dot(ctx.fill_dir).max(0.0) * studio.fill * ao;
    let fill_tint = [0.86, 0.94, 1.06];

    // Hemisphere sky/ground ambient modulated by AO
    let hemi = (0.5 + 0.5 * n.dot(cam.up)).clamp(0.0, 1.0);
    let amb_scale = studio.ambient * ao;
    let sky_rgb = [0.92, 0.96, 1.04];
    let gnd_rgb = [0.55, 0.52, 0.48];
    let hemi_rgb = [
        (gnd_rgb[0] + (sky_rgb[0] - gnd_rgb[0]) * hemi) * amb_scale,
        (gnd_rgb[1] + (sky_rgb[1] - gnd_rgb[1]) * hemi) * amb_scale,
        (gnd_rgb[2] + (sky_rgb[2] - gnd_rgb[2]) * hemi) * amb_scale,
    ];

    // Blinn-Phong specular highlight from key light
    let spec = if studio.specular > 0.0 && studio.shininess > 0.0 && ndotl_key > 0.0 {
        let half_v = (ctx.key_dir + v).normalized().unwrap_or(ctx.key_dir);
        let ndoth = n.dot(half_v).max(0.0);
        if ndoth > 0.0 {
            blaze_math::pow(ndoth, studio.shininess) * studio.specular * shadow
        } else {
            0.0
        }
    } else {
        0.0
    };

    // Subtle Fresnel rim highlight along silhouettes
    let rim = if studio.rim > 0.0 {
        let fresnel = blaze_math::pow((1.0 - n.dot(v).max(0.0)).clamp(0.0, 1.0), 3.0);
        let rim_dir_mask = 0.35 + 0.65 * n.dot(ctx.rim_dir).max(0.0);
        fresnel * rim_dir_mask * studio.rim * ao
    } else {
        0.0
    };
    let rim_tint = [0.95, 0.98, 1.04];

    // Optional movable studio strip lights (exact no-op when all slots are `None`).
    let strip_roughness = (1.0 / blaze_math::sqrt(studio.shininess.max(1.0))).clamp(0.08, 0.85);
    let strip_contrib = eval_strip_lights(cam, &studio.strip_lights, n, v, strip_roughness, ao);

    let mut out = [0.0f64; 4];
    for c in 0..3 {
        let diff = albedo[c] * (ndotl_key * key_tint[c] + ndotl_fill * fill_tint[c] + hemi_rgb[c]);
        let mut col = diff + spec * key_tint[c] + rim * rim_tint[c];
        if let Some((s_diff, s_spec)) = strip_contrib {
            col += albedo[c] * s_diff[c] + s_spec[c];
        }
        out[c] = col.clamp(0.0, 1.0);
    }
    out[3] = 1.0;
    out
}

/// Class-A reflection-line zebra analysis shared by both the SDF tracer and the mesh renderer.
///
/// Reflects the viewing ray `dir` across unit normal `n`, projects onto `zebra.axis` (in camera
/// space by default, or world space when `zebra.world_space` is true), maps the elevation via
/// `asin` for uniform angular stripe spacing across the reflection hemisphere, and applies a
/// smoothstep anti-aliased contrast ramp between `zebra.dark` and `zebra.light`.
pub(crate) fn shade_zebra(cam: &Camera, dir: Vec3, n: Vec3, zebra: &ZebraOpts) -> [f64; 4] {
    let r = (dir - n * (2.0 * dir.dot(n))).normalized().unwrap_or(n);
    let axis = Vec3::new(zebra.axis[0], zebra.axis[1], zebra.axis[2])
        .normalized()
        .unwrap_or(Vec3::new(0.0, 0.0, 1.0));
    let proj = if zebra.world_space {
        r.dot(axis)
    } else {
        let r_cam = Vec3::new(r.dot(cam.right), r.dot(cam.up), -r.dot(cam.forward));
        r_cam.dot(axis)
    };
    let coord = blaze_math::asin(proj.clamp(-1.0, 1.0)) / core::f64::consts::PI;
    let s = blaze_math::sin(2.0 * core::f64::consts::PI * (coord * zebra.stripes + zebra.phase));
    let sharp = if zebra.sharpness.is_finite() { zebra.sharpness.max(0.0) } else { 8.0 };
    let t = (0.5 + 0.5 * (s * sharp).clamp(-1.0, 1.0)).clamp(0.0, 1.0);
    let band = t * t * (3.0 - 2.0 * t);

    let mut out = [0.0f64; 4];
    for c in 0..3 {
        let d = f64::from(zebra.dark[c]) / 255.0;
        let l = f64::from(zebra.light[c]) / 255.0;
        out[c] = (d + (l - d) * band).clamp(0.0, 1.0);
    }
    out[3] = 1.0;
    out
}

/// Evaluate up to 4 analytical rectangular studio strip lights (`None` when all slots are inactive,
/// guaranteeing zero arithmetic change to default studio renders).
///
/// For each active strip light, transforms its camera-space `direction` and `tangent` into world
/// space, clamps the reflection ray `R = reflect(-V, N)` onto the strip rectangle
/// `[-half_length, half_length] × [-half_width, half_width]` at unit distance, and accumulates a
/// roughness-aware specular ribbon plus subtle wrapped diffuse fill.
pub(crate) fn eval_strip_lights(
    cam: &Camera,
    strips: &[Option<StripLight>; 4],
    n: Vec3,
    v: Vec3,
    roughness: f64,
    ao: f64,
) -> Option<([f64; 3], [f64; 3])> {
    let mut any = false;
    for slot in strips {
        if slot.is_some() {
            any = true;
            break;
        }
    }
    if !any {
        return None;
    }

    let ndotv = n.dot(v).max(0.0);
    let r_vec = (n * (2.0 * n.dot(v)) - v).normalized().unwrap_or(n);
    let rough = roughness.clamp(0.04, 1.0);
    let sigma = (0.035 + 0.35 * rough * rough).clamp(0.035, 0.55);
    let inv_two_sigma_sq = 0.5 / (sigma * sigma);
    let fresnel = 0.25 + 0.75 * blaze_math::pow((1.0 - ndotv).clamp(0.0, 1.0), 3.0);

    let mut diff_rgb = [0.0f64; 3];
    let mut spec_rgb = [0.0f64; 3];

    for slot in strips {
        let Some(strip) = slot else { continue };
        if !(strip.intensity > 0.0 && strip.intensity.is_finite()) {
            continue;
        }
        let d_raw = cam.right * strip.direction[0] + cam.up * strip.direction[1] - cam.forward * strip.direction[2];
        let Some(dir_w) = d_raw.normalized() else { continue };

        let t_raw = cam.right * strip.tangent[0] + cam.up * strip.tangent[1] - cam.forward * strip.tangent[2];
        let t_proj = t_raw - dir_w * t_raw.dot(dir_w);
        let u_axis = match t_proj.normalized() {
            Some(u) => u,
            None => {
                let fallback = if dir_w.dot(cam.right).abs() < 0.9 { cam.right } else { cam.up };
                (fallback - dir_w * fallback.dot(dir_w)).normalized().unwrap_or(cam.right)
            }
        };
        let v_axis = dir_w.cross(u_axis).normalized().unwrap_or(cam.up);

        let hl = if strip.half_length.is_finite() { strip.half_length.max(0.0) } else { 0.0 };
        let hw = if strip.half_width.is_finite() { strip.half_width.max(0.0) } else { 0.0 };

        // Project reflection ray onto the strip's tangent plane at unit distance and clamp to rectangle.
        let r_dot_d = r_vec.dot(dir_w);
        let (u_coord, v_coord) = if r_dot_d > 1e-4 {
            let inv = 1.0 / r_dot_d;
            ((r_vec.dot(u_axis) * inv).clamp(-hl, hl), (r_vec.dot(v_axis) * inv).clamp(-hw, hw))
        } else {
            let inv = 1e4;
            ((r_vec.dot(u_axis) * inv).clamp(-hl, hl), (r_vec.dot(v_axis) * inv).clamp(-hw, hw))
        };

        let l_rep = (dir_w + u_axis * u_coord + v_axis * v_coord).normalized().unwrap_or(dir_w);
        let cos_ang = r_vec.dot(l_rep).clamp(-1.0, 1.0);
        let d_ang = blaze_math::acos(cos_ang);
        let ndotl_rep = n.dot(l_rep).max(0.0);
        let horizon = (ndotl_rep * 4.0).clamp(0.0, 1.0);
        let spec_lobe = blaze_math::exp(-(d_ang * d_ang) * inv_two_sigma_sq)
            * strip.intensity
            * fresnel
            * horizon
            * (0.5 + 0.5 * ao);
        let diff_wrap = ((n.dot(dir_w) + 0.25) / 1.25).clamp(0.0, 1.0) * 0.18 * strip.intensity * ao;

        for c in 0..3 {
            let col = if strip.colour[c].is_finite() { strip.colour[c].max(0.0) } else { 0.0 };
            diff_rgb[c] += diff_wrap * col;
            spec_rgb[c] += spec_lobe * col;
        }
    }

    Some((diff_rgb, spec_rgb))
}

/// Procedural studio metallic/ceramic CAD matcap from camera-space normal `(nx, ny, nz)`.
fn shade_matcap(cam: &Camera, ctx: &ShadeContext, n: Vec3) -> [f64; 4] {
    let nx = n.dot(cam.right);
    let ny = n.dot(cam.up);
    let nz = n.dot(cam.forward * -1.0).clamp(0.0, 1.0);

    // Studio dome gradient (cool overhead softbox -> warm lower bounce)
    let up_t = (ny * 0.5 + 0.5).clamp(0.0, 1.0);
    let dome = [
        0.24 + 0.52 * up_t,
        0.25 + 0.54 * up_t,
        0.28 + 0.56 * up_t,
    ];

    // Primary overhead-left softbox reflection lobe
    let l1 = Vec3::new(-0.35, 0.58, 0.735).normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0));
    let nv = Vec3::new(nx, ny, nz);
    let d1 = nv.dot(l1).max(0.0);
    let lobe1 = blaze_math::pow(d1, 24.0) * 0.55 + d1 * 0.25;

    // Secondary crisp right-edge strip reflection
    let l2 = Vec3::new(0.70, 0.15, 0.70).normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0));
    let d2 = nv.dot(l2).max(0.0);
    let lobe2 = blaze_math::pow(d2, 14.0) * 0.22;

    // Subtle grazing Fresnel sheen
    let rim = blaze_math::pow((1.0 - nz).clamp(0.0, 1.0), 3.0) * 0.28;

    let mut out = [0.0f64; 4];
    for c in 0..3 {
        let tint = 0.35 + 0.65 * ctx.base_norm[c];
        let v = (dome[c] * tint + lobe1 + lobe2 * 0.92 + rim * 0.95).clamp(0.0, 1.0);
        out[c] = v;
    }
    out[3] = 1.0;
    out
}
